//! Global low-level mouse and keyboard hooks.
//!
//! Windows calls a low-level hook on the thread that installed it, and only
//! while that thread waits for messages, so the hooks get a thread of their
//! own. Every keystroke and mouse movement on the machine waits for the hook
//! to return, and Windows silently removes a hook that takes too long
//! (about 300 ms by default), without saying so. So this thread runs ahead of nearly
//! everything else, and checks every so often that the hooks are still hearing about input.

use std::cell::{Cell, RefCell};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_core::geometry::Point;
use hk_core::input::MouseButton;
use hk_core::switcher::LocalEvent;
use windows::Win32::Foundation::{ERROR_SUCCESS, HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER,
    WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1,
};
use windows::core::w;

use super::{Occasionally, cursor, keymap};
use crate::{Error, InputHandler};

/// How often to check that the hooks are still hearing about input, in milliseconds.
const CHECK_HOOKS_EVERY_MS: u32 = 2_000;
/// Input this much newer than the last the hooks heard about means they missed it.
const MISSED_AFTER_MS: u32 = 1_000;
/// How often to put the hooks back at most. They also miss input for good reason, like while
/// an administrator's program is in front, and that needn't be happening all the time.
const PUT_BACK_AT_MOST_EVERY: Duration = Duration::from_secs(5);
/// An event reaching the hooks this late means this computer is holding input up.
const LATE_EVENT_MS: u32 = 150;
/// The hooks taking this long over one event would be holding up all input.
const SLOW_HANDLER: Duration = Duration::from_millis(50);

thread_local! {
    // Hook procedures receive no user data, so they find the handler here.
    static HANDLER: RefCell<Option<InputHandler>> = const { RefCell::new(None) };
    static PREVIOUS_MOVE: Cell<Option<POINT>> = const { Cell::new(None) };
    /// When either hook last heard about anything, by Windows' clock.
    static LAST_HEARD: Cell<u32> = const { Cell::new(0) };
    static LAST_PUT_BACK: Cell<Option<Instant>> = const { Cell::new(None) };
    static LATE_REPORT: Occasionally = const { Occasionally::every(Duration::from_secs(10)) };
    static SLOW_REPORT: Occasionally = const { Occasionally::every(Duration::from_secs(10)) };
}

/// Logs from another thread, since writing to a file could hold up this one, and with it all
/// input on the computer. This only happens when something has already gone wrong.
fn report(log: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().spawn(log);
}

/// Windows waits this long for a hook, and the setting is often changed by other programs.
fn log_hook_timeout() {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the size given is that of the value it fills in.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Control Panel\\Desktop"),
            w!("LowLevelHooksTimeout"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    if result == ERROR_SUCCESS {
        // Windows uses a whole second at most, whatever this says.
        tracing::info!(ms = value, "this computer has its own hook timeout set");
    }
}

/// Whether input has happened that the hooks should have heard about but didn't. Both come
/// from a clock that goes back to zero every 49 days.
fn hooks_missed_input(last_input: u32, last_heard: u32) -> bool {
    let newer_by = last_input.wrapping_sub(last_heard);
    newer_by > MISSED_AFTER_MS && newer_by < u32::MAX / 2
}

/// Puts the hooks back if they've missed input. The new ones go in first, so that nothing is
/// missed while swapping them. Removing the old ones says whether Windows already had.
fn look_after_hooks(hooks: &mut [HHOOK; 2]) {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: the structure states its own size.
    if !unsafe { GetLastInputInfo(&raw mut info) }.as_bool()
        || !hooks_missed_input(info.dwTime, LAST_HEARD.get())
        || LAST_PUT_BACK
            .get()
            .is_some_and(|at| at.elapsed() < PUT_BACK_AT_MOST_EVERY)
    {
        return;
    }
    LAST_PUT_BACK.set(Some(Instant::now()));
    match install() {
        Ok(new) => {
            let old = std::mem::replace(hooks, new);
            let removed = old
                .into_iter()
                .filter(|&hook| unsafe { UnhookWindowsHookEx(hook) }.is_err())
                .count();
            // SAFETY: only reads a clock.
            LAST_HEARD.set(unsafe { GetTickCount() });
            if removed > 0 {
                report(move || {
                    tracing::warn!(
                        removed,
                        "Windows had removed input hooks for taking too long. They're back"
                    );
                });
            } else {
                report(|| tracing::debug!("the input hooks missed some input"));
            }
        }
        Err(error) => report(move || tracing::warn!(%error, "can't put the input hooks back")),
    }
}

/// Keeps track of when the hooks last heard anything, and whether it reached them in time.
fn note_event(event_time: u32) {
    // SAFETY: only reads a clock.
    let now = unsafe { GetTickCount() };
    LAST_HEARD.set(now);
    let late_by = now.wrapping_sub(event_time);
    if (LATE_EVENT_MS..u32::MAX / 2).contains(&late_by) && LATE_REPORT.with(Occasionally::due) {
        report(move || {
            tracing::warn!(
                ms = late_by,
                "input took a long time to reach the hooks. This computer, or another program \
                 watching the keyboard and mouse, is holding it up"
            );
        });
    }
}

/// Keeps the hooks installed. Dropping it removes them.
pub(super) struct Hooks {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Hooks {
    pub(super) fn start(handler: InputHandler) -> Result<Self, Error> {
        log_hook_timeout();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("input-hooks".into())
            .spawn(move || {
                // This only ever runs for a moment at a time, and everything waits for it.
                // SAFETY: this only concerns this thread, and a failure leaves it as it is.
                let _ =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
                HANDLER.with_borrow_mut(|slot| *slot = Some(handler));
                // SAFETY: only reads a clock.
                LAST_HEARD.set(unsafe { GetTickCount() });
                let mut hooks = match install() {
                    Ok(hooks) => hooks,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                // Create this thread's message queue before anyone posts to it.
                let mut message = MSG::default();
                let _ = unsafe { PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE) };
                // Sharing input still works if the cursor can't be hidden.
                match unsafe { GetModuleHandleW(None) } {
                    Ok(module) => {
                        if let Err(error) = cursor::create(HINSTANCE(module.0)) {
                            tracing::warn!(%error, "can't create the window that hides the cursor");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "can't get this program's module handle"),
                }
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                // With no window and no function, this arrives as a message below.
                // SAFETY: it needs nothing, and goes when the thread does.
                let check_hooks = unsafe { SetTimer(None, 0, CHECK_HOOKS_EVERY_MS, None) };

                // GetMessageW returns 0 for WM_QUIT and -1 on error. The hooks
                // run inside it; dispatching is for the cursor window's messages.
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                    if message.message == WM_TIMER && message.wParam.0 == check_hooks {
                        look_after_hooks(&mut hooks);
                        continue;
                    }
                    unsafe { DispatchMessageW(&message) };
                }

                cursor::destroy();
                for hook in hooks {
                    let _ = unsafe { UnhookWindowsHookEx(hook) };
                }
            })
            .map_err(|error| Error::Hook(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(Error::Hook("the hook thread stopped unexpectedly".into())),
        }
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        // Ending the message loop makes the thread remove the hooks and exit.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn install() -> Result<[HHOOK; 2], Error> {
    let hook_error = |error: windows::core::Error| Error::Hook(error.message());
    let module = unsafe { GetModuleHandleW(None) }.map_err(hook_error)?;
    let instance = Some(HINSTANCE(module.0));
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), instance, 0) }
        .map_err(hook_error)?;
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), instance, 0) } {
        Ok(keyboard) => Ok([mouse, keyboard]),
        Err(error) => {
            let _ = unsafe { UnhookWindowsHookEx(mouse) };
            Err(hook_error(error))
        }
    }
}

/// Runs the handler, returning whether to hide the event from Windows.
fn dispatch(event: LocalEvent) -> bool {
    let started = Instant::now();
    let hide = HANDLER.with(|slot| match slot.try_borrow_mut() {
        Ok(mut handler) => handler.as_mut().is_some_and(|handler| handler(event)),
        // The handler is already running further up this thread's stack; let
        // this nested event through rather than panic.
        Err(_) => false,
    });
    let took = started.elapsed();
    if took >= SLOW_HANDLER && SLOW_REPORT.with(Occasionally::due) {
        report(move || {
            tracing::warn!(
                ms = took.as_millis() as u64,
                "handling one input event took too long"
            );
        });
    }
    hide
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to an MSLLHOOKSTRUCT that is
        // valid for the duration of this call.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        note_event(info.time);
        let message = wparam.0 as u32;
        // Worked out for injected moves as well, which also move the cursor.
        let from = (message == WM_MOUSEMOVE).then(|| cursor_before(info));
        // Injected events are the ones we replay for a peer; capturing them
        // again would feed them back into the switcher.
        let injected = info.flags & LLMHF_INJECTED != 0;
        if !injected
            && let Some(event) = mouse_event(message, info, from)
            && dispatch(event)
        {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT that is
        // valid for the duration of this call.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        note_event(info.time);
        let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
        if !injected {
            let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
            let event = LocalEvent::Key {
                code: keymap::from_windows(info.vkCode, info.scanCode, extended),
                pressed: info.flags.0 & LLKHF_UP.0 == 0,
            };
            if dispatch(event) {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Where this move starts from. That's where the cursor is now, if Windows hasn't
/// moved it yet when a low-level hook runs, and otherwise where the previous move
/// went. A cursor already at the destination gives the second answer, which also
/// holds when the cursor is pinned against an edge and isn't going anywhere.
fn cursor_before(info: &MSLLHOOKSTRUCT) -> Point {
    let previous = PREVIOUS_MOVE.replace(Some(info.pt));
    let mut cursor = info.pt;
    let _ = unsafe { GetCursorPos(&mut cursor) };
    let from = if cursor == info.pt {
        previous.unwrap_or(cursor)
    } else {
        cursor
    };
    Point::new(f64::from(from.x), f64::from(from.y))
}

fn mouse_event(message: u32, info: &MSLLHOOKSTRUCT, from: Option<Point>) -> Option<LocalEvent> {
    let high_word = (info.mouseData >> 16) as u16;
    // Wheel deltas are signed multiples of 120 per notch.
    let notches = f64::from(high_word as i16) / 120.0;
    let side_button = if high_word == XBUTTON1 {
        MouseButton::Back
    } else {
        MouseButton::Forward
    };
    let button = |button, pressed| Some(LocalEvent::Button { button, pressed });

    match message {
        WM_MOUSEMOVE => {
            let to = Point::new(f64::from(info.pt.x), f64::from(info.pt.y));
            Some(LocalEvent::Motion {
                from: from.unwrap_or(to),
                to,
            })
        }
        WM_LBUTTONDOWN => button(MouseButton::Left, true),
        WM_LBUTTONUP => button(MouseButton::Left, false),
        WM_RBUTTONDOWN => button(MouseButton::Right, true),
        WM_RBUTTONUP => button(MouseButton::Right, false),
        WM_MBUTTONDOWN => button(MouseButton::Middle, true),
        WM_MBUTTONUP => button(MouseButton::Middle, false),
        WM_XBUTTONDOWN => button(side_button, true),
        WM_XBUTTONUP => button(side_button, false),
        WM_MOUSEWHEEL => Some(LocalEvent::Scroll {
            dx: 0.0,
            dy: notches,
        }),
        WM_MOUSEHWHEEL => Some(LocalEvent::Scroll {
            dx: notches,
            dy: 0.0,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_that_are_up_to_date_have_missed_nothing() {
        assert!(!hooks_missed_input(5_000, 5_000));
        assert!(!hooks_missed_input(5_000, 4_500), "a moment apart");
        assert!(!hooks_missed_input(4_000, 5_000), "they heard of it last");
    }

    #[test]
    fn input_the_hooks_never_heard_about_is_noticed() {
        assert!(hooks_missed_input(10_000, 5_000));
    }

    /// The clock goes back to zero every 49 days or so.
    #[test]
    fn missing_input_is_noticed_across_the_clock_going_back_to_zero() {
        assert!(hooks_missed_input(4_000, u32::MAX - 1_000));
        assert!(!hooks_missed_input(100, u32::MAX - 100));
        assert!(!hooks_missed_input(u32::MAX - 100, 100), "the other way");
    }

    /// Makes it look as if input has happened since the hooks last heard anything.
    fn pretend_input_was_missed() {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        assert!(unsafe { GetLastInputInfo(&raw mut info) }.as_bool());
        LAST_HEARD.set(info.dwTime.wrapping_sub(60_000));
        LAST_PUT_BACK.set(None);
    }

    /// Real hooks, for a moment. With no message loop here, any input that happened in that
    /// moment would wait for this thread, until the hooks go or the thread does.
    #[test]
    #[ignore = "installs real input hooks: see scripts/run-isolated.ps1"]
    fn hooks_that_missed_input_are_put_back_even_if_windows_removed_them() {
        let first = install().unwrap();
        let mut hooks = first;

        pretend_input_was_missed();
        look_after_hooks(&mut hooks);
        assert_ne!(hooks, first, "new ones go in");
        assert!(
            unsafe { UnhookWindowsHookEx(first[0]) }.is_err(),
            "the old ones have gone"
        );

        // Not again straight away, however much is missed.
        let second = hooks;
        LAST_HEARD.set(LAST_HEARD.get().wrapping_sub(60_000));
        look_after_hooks(&mut hooks);
        assert_eq!(hooks, second);

        // What Windows does to hooks that take too long, without saying so.
        for hook in second {
            unsafe { UnhookWindowsHookEx(hook) }.unwrap();
        }
        pretend_input_was_missed();
        look_after_hooks(&mut hooks);
        assert_ne!(hooks, second, "they come back");
        for hook in hooks {
            unsafe { UnhookWindowsHookEx(hook) }.unwrap();
        }
    }

    #[test]
    fn something_due_now_and_then_is_due_once_for_a_while() {
        let report = Occasionally::every(Duration::from_secs(3600));
        assert!(report.due());
        assert!(!report.due());
        let report = Occasionally::every(Duration::ZERO);
        assert!(report.due());
        assert!(report.due());
    }
}
