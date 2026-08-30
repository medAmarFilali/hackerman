//! Global low-level mouse and keyboard hooks.
//!
//! Windows calls a low-level hook on the thread that installed it, and only
//! while that thread waits for messages, so the hooks get a thread of their
//! own. Every keystroke and mouse movement on the machine waits for the hook
//! to return, and Windows silently removes a hook that takes too long
//! (about 300 ms by default).

use std::cell::{Cell, RefCell};
use std::sync::mpsc;
use std::thread::JoinHandle;

use hk_core::geometry::Point;
use hk_core::input::MouseButton;
use hk_core::switcher::LocalEvent;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_USER, WM_XBUTTONDOWN,
    WM_XBUTTONUP, XBUTTON1,
};

use super::{cursor, keymap};
use crate::{Error, InputHandler};

thread_local! {
    // Hook procedures receive no user data, so they find the handler here.
    static HANDLER: RefCell<Option<InputHandler>> = const { RefCell::new(None) };
    static PREVIOUS_MOVE: Cell<Option<POINT>> = const { Cell::new(None) };
}

/// Keeps the hooks installed. Dropping it removes them.
pub(super) struct Hooks {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Hooks {
    pub(super) fn start(handler: InputHandler) -> Result<Self, Error> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("input-hooks".into())
            .spawn(move || {
                HANDLER.with_borrow_mut(|slot| *slot = Some(handler));
                let hooks = match install() {
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

                // GetMessageW returns 0 for WM_QUIT and -1 on error. The hooks
                // run inside it; dispatching is for the cursor window's messages.
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
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
    HANDLER.with(|slot| match slot.try_borrow_mut() {
        Ok(mut handler) => handler.as_mut().is_some_and(|handler| handler(event)),
        // The handler is already running further up this thread's stack; let
        // this nested event through rather than panic.
        Err(_) => false,
    })
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to an MSLLHOOKSTRUCT that is
        // valid for the duration of this call.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
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
