//! Replays input from a peer with `SendInput`, as if it came from this
//! machine's own devices.
//!
//! Windows blocks input sent to windows of elevated (administrator) programs
//! unless the sender is elevated too. The system's input helper is, which is why the
//! agent hands input to it while it's there: see the `relay` module.
//!
//! `SendInput` waits for every program with a low-level input hook on the computer, and any
//! of them can be slow. So it gets a thread of its own: however long it takes, the connection
//! that the input came in on keeps going, which it needs to do to stay up. And when it does
//! catch up, the cursor goes to where it should be by then, not through everywhere it has been.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use hk_core::geometry::Rect;
use hk_core::input::{InputEvent, MouseButton};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
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

use super::{Occasionally, keymap};

/// `SendInput` taking this long means something on this computer is holding input up.
const SLOW_TO_INJECT: Duration = Duration::from_millis(100);
/// How long to wait for the last of the input when stopping.
const FINISH_TIMEOUT: Duration = Duration::from_secs(1);

thread_local! {
    static SLOW_REPORT: Occasionally = const { Occasionally::every(Duration::from_secs(10)) };
    static REFUSED_REPORT: Occasionally = const { Occasionally::every(Duration::from_secs(10)) };
}

enum Job {
    Replay(InputEvent),
    /// Says when everything before this has been replayed.
    Finished(Sender<()>),
}

/// Hands input to the thread that replays it.
#[derive(Debug, Clone)]
pub(super) struct Replayer {
    jobs: Sender<Job>,
}

impl Replayer {
    /// `replay` is what's done with each event on the thread, like [`inject`]. The thread
    /// stops with the last of these.
    pub(super) fn start(replay: impl Fn(&InputEvent) + Send + 'static) -> Self {
        let (jobs, queue) = mpsc::channel();
        let started = std::thread::Builder::new()
            .name("input-replay".into())
            .spawn(move || {
                // SAFETY: this only concerns this thread, and a failure just leaves it as it is.
                let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
                run(&queue, replay);
            });
        if let Err(error) = started {
            tracing::error!(%error, "can't start the thread that replays input");
        }
        Self { jobs }
    }

    pub(super) fn replay(&self, event: InputEvent) {
        let _ = self.jobs.send(Job::Replay(event));
    }

    pub(super) fn finish(&self) {
        let (done, finished) = mpsc::channel();
        if self.jobs.send(Job::Finished(done)).is_ok() {
            let _ = finished.recv_timeout(FINISH_TIMEOUT);
        }
    }
}

/// Goes through the jobs until nobody can send any more. A position is skipped if the next
/// thing to do is another position. Anything else keeps its place: a click has to land where
/// the cursor was when it was made.
fn run(queue: &Receiver<Job>, mut replay: impl FnMut(&InputEvent)) {
    let is_position = |event: &InputEvent| matches!(event, InputEvent::PointerPosition { .. });
    let mut next = None;
    loop {
        let job = match next.take() {
            Some(job) => job,
            None => match queue.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        match job {
            Job::Finished(done) => {
                let _ = done.send(());
            }
            Job::Replay(event) => {
                if is_position(&event) {
                    match queue.try_recv() {
                        Ok(Job::Replay(newer)) if is_position(&newer) => {
                            next = Some(Job::Replay(newer));
                            continue;
                        }
                        Ok(other) => next = Some(other),
                        Err(_) => {}
                    }
                }
                replay(&event);
            }
        }
    }
}

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

    let started = Instant::now();
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    let took = started.elapsed();
    if sent as usize != inputs.len() && REFUSED_REPORT.with(Occasionally::due) {
        tracing::warn!(
            error = %windows::core::Error::from_thread(),
            "Windows refused input from another PC. It does that while an administrator's \
             program is in front, and on some protected screens"
        );
    }
    if took >= SLOW_TO_INJECT && SLOW_REPORT.with(Occasionally::due) {
        tracing::warn!(
            ms = took.as_millis() as u64,
            "Windows took a long time to accept input from another PC. Another program \
             watching the keyboard and mouse may be holding it up"
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

    const fn position(x: f64) -> InputEvent {
        InputEvent::PointerPosition { x, y: 0.0 }
    }

    const CLICK: InputEvent = InputEvent::Button {
        button: MouseButton::Left,
        pressed: true,
    };

    /// All of these queued up at once, as they would behind a slow `SendInput`.
    fn replayed(events: &[InputEvent]) -> Vec<InputEvent> {
        let (jobs, queue) = mpsc::channel();
        for &event in events {
            jobs.send(Job::Replay(event)).unwrap();
        }
        drop(jobs);
        let mut seen = Vec::new();
        run(&queue, |event| seen.push(*event));
        seen
    }

    #[test]
    fn only_the_newest_of_several_positions_in_a_row_matters() {
        assert_eq!(
            replayed(&[position(1.0), position(2.0), position(3.0)]),
            [position(3.0)]
        );
    }

    #[test]
    fn a_click_lands_where_the_cursor_was_when_it_was_made() {
        assert_eq!(
            replayed(&[
                position(1.0),
                position(2.0),
                CLICK,
                position(3.0),
                position(4.0)
            ]),
            [position(2.0), CLICK, position(4.0)]
        );
    }

    #[test]
    fn nothing_but_positions_is_ever_skipped() {
        let key = InputEvent::Key {
            code: hk_core::input::KeyCode::KeyA,
            pressed: true,
        };
        let all = [key, CLICK, key, CLICK];
        assert_eq!(replayed(&all), all);
    }

    #[test]
    fn finishing_waits_for_what_came_before_it() {
        let (jobs, queue) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        jobs.send(Job::Replay(CLICK)).unwrap();
        jobs.send(Job::Finished(done)).unwrap();
        drop(jobs);
        let mut seen = Vec::new();
        run(&queue, |event| seen.push(*event));
        assert_eq!(seen, [CLICK]);
        assert_eq!(finished.try_recv(), Ok(()));
    }

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
