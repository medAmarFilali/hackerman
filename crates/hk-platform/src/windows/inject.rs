//! Replays input from a peer with `SendInput`, as if it came from this
//! machine's own devices.
//!
//! Windows blocks input sent to windows of elevated (administrator) programs
//! unless the sender is elevated too.

use hk_core::geometry::Rect;
use hk_core::input::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use super::keymap;

pub(super) fn inject(event: &InputEvent) {
    let mut inputs = Vec::with_capacity(2);
    match *event {
        InputEvent::PointerPosition { x, y } => {
            let (dx, dy) = normalize(current_desktop(), x, y);
            inputs.push(mouse(
                dx,
                dy,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                0,
            ));
        }
        InputEvent::PointerMotion { dx, dy } => {
            inputs.push(mouse(
                dx.round() as i32,
                dy.round() as i32,
                MOUSEEVENTF_MOVE,
                0,
            ));
        }
        InputEvent::Button { button, pressed } => {
            let (flags, data) = button_flags(button, pressed);
            inputs.push(mouse(0, 0, flags, data));
        }
        InputEvent::Scroll { dx, dy } => {
            // Windows wants 120 units per wheel notch, as a signed value in a u32.
            let units = |notches: f64| (notches * 120.0).round() as i32 as u32;
            if dy != 0.0 {
                inputs.push(mouse(0, 0, MOUSEEVENTF_WHEEL, units(dy)));
            }
            if dx != 0.0 {
                inputs.push(mouse(0, 0, MOUSEEVENTF_HWHEEL, units(dx)));
            }
        }
        InputEvent::Key { code, pressed } => {
            let Some(stroke) = keymap::to_windows(code) else {
                return;
            };
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if stroke.vk == 0 {
                // Sending the physical scan code lets this machine's own
                // keyboard layout decide which character it types.
                flags |= KEYEVENTF_SCANCODE;
            }
            if stroke.extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(stroke.vk),
                        wScan: stroke.scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }

    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        tracing::debug!(
            error = %windows::core::Error::from_thread(),
            "Windows rejected injected input"
        );
    }
}

/// A mouse movement of zero distance.
pub(super) fn nudge_nowhere() {
    unsafe {
        SendInput(
            &[mouse(0, 0, MOUSEEVENTF_MOVE, 0)],
            size_of::<INPUT>() as i32,
        )
    };
}

/// The bounding box of all monitors, in physical pixels. Read fresh each time
/// because monitors can come and go at any moment.
fn current_desktop() -> Rect {
    let metric = |index| f64::from(unsafe { GetSystemMetrics(index) });
    Rect::new(
        metric(SM_XVIRTUALSCREEN),
        metric(SM_YVIRTUALSCREEN),
        metric(SM_CXVIRTUALSCREEN),
        metric(SM_CYVIRTUALSCREEN),
    )
}

/// Converts desktop pixels to the 0..=65535 range that absolute
/// `MOUSEEVENTF_VIRTUALDESK` moves use, aiming at the pixel's centre so
/// rounding never lands on its neighbour.
fn normalize(desktop: Rect, x: f64, y: f64) -> (i32, i32) {
    let scale =
        |offset: f64, length: f64| ((offset + 0.5) * 65536.0 / length).clamp(0.0, 65535.0) as i32;
    (
        scale(x - desktop.x, desktop.width),
        scale(y - desktop.y, desktop.height),
    )
}

fn button_flags(button: MouseButton, pressed: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    let pick = |down, up| if pressed { down } else { up };
    match button {
        MouseButton::Left => (pick(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP), 0),
        MouseButton::Right => (pick(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP), 0),
        MouseButton::Middle => (pick(MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP), 0),
        MouseButton::Back => (
            pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
            u32::from(XBUTTON1),
        ),
        MouseButton::Forward => (
            pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
            u32::from(XBUTTON2),
        ),
    }
}

fn mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_positions_cover_the_whole_range() {
        let desktop = Rect::new(-1920.0, 0.0, 3840.0, 1080.0);
        assert_eq!(normalize(desktop, -1920.0, 0.0), (8, 30));
        assert_eq!(normalize(desktop, 1919.0, 1079.0), (65527, 65505));
        // Off-desktop positions are pinned to the edge rather than wrapping.
        assert_eq!(normalize(desktop, 5000.0, -10.0), (65535, 0));
    }

    #[test]
    fn normalized_positions_map_back_to_the_same_pixel() {
        let desktop = Rect::new(0.0, 0.0, 2560.0, 1440.0);
        for x in [0.0, 1.0, 1279.0, 2558.0, 2559.0] {
            let (normalized, _) = normalize(desktop, x, 0.0);
            // This is how Windows maps a normalized coordinate back to a pixel.
            let pixel = (f64::from(normalized) * desktop.width / 65536.0).floor();
            assert_eq!(pixel, x);
        }
    }
}
