//! Hiding the cursor while input goes to another machine.
//!
//! Windows has no call to hide the cursor everywhere. The common trick of
//! replacing the system's cursors with blank ones leaves it invisible for good if
//! the program crashes. Instead, the parked cursor sits over a tiny, nearly
//! transparent window of ours that asks for no cursor at all. If this program
//! ends for any reason, the window goes with it and the cursor comes back.
//!
//! The window belongs to the input hook thread, which already waits for messages.
//! Other threads, and the hook itself, only post to it, so nothing here can hold
//! up the machine's input.

use std::sync::atomic::{AtomicPtr, Ordering};

use hk_core::geometry::Point;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, LWA_ALPHA, PostMessageW,
    RegisterClassW, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SetCursor, SetCursorPos, SetLayeredWindowAttributes, SetWindowPos, WM_APP,
    WM_SETCURSOR, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::w;

use super::inject;

/// Wide enough that the cursor is well inside it.
const SIZE: i32 = 64;

const SHOW_AT: u32 = WM_APP + 1;
const HIDE: u32 = WM_APP + 2;
const HIDE_AND_MOVE: u32 = WM_APP + 3;

static WINDOW: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

fn window() -> Option<HWND> {
    let handle = WINDOW.load(Ordering::Acquire);
    (!handle.is_null()).then_some(HWND(handle))
}

/// Creates the window. Must be called on the thread that will process its messages.
pub(super) fn create(instance: HINSTANCE) -> windows::core::Result<()> {
    let class_name = w!("HackermanCursorCloak");
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class_name,
        ..Default::default()
    };
    // Fails harmlessly if the hooks were started before in this process.
    unsafe { RegisterClassW(&class) };

    // SAFETY: the class was registered above, and the pointers it uses are static.
    let window = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name,
            w!(""),
            WS_POPUP,
            0,
            0,
            SIZE,
            SIZE,
            None,
            None,
            Some(instance),
            None,
        )
    }?;
    // Nearly but not fully transparent: Windows skips fully transparent windows
    // when working out what is under the cursor.
    unsafe { SetLayeredWindowAttributes(window, COLORREF(0), 1, LWA_ALPHA) }?;
    WINDOW.store(window.0, Ordering::Release);
    Ok(())
}

/// Removes the window. Must be called on the thread that created it.
pub(super) fn destroy() {
    if let Some(window) = window() {
        WINDOW.store(std::ptr::null_mut(), Ordering::Release);
        let _ = unsafe { DestroyWindow(window) };
    }
}

/// Moves the cursor here and hides it, from the window's thread. Says whether there's a window.
pub(super) fn hide_at(at: Point) -> bool {
    post_at(SHOW_AT, at)
}

pub(super) fn show() {
    if let Some(window) = window() {
        let _ = unsafe { PostMessageW(Some(window), HIDE, WPARAM(0), LPARAM(0)) };
    }
}

/// Shows the cursor again, here, with the move made from the window's thread. Says whether
/// there's a window. A thread waiting on that one, as the input helper's can, can't hold it up.
pub(super) fn show_at(to: Point) -> bool {
    post_at(HIDE_AND_MOVE, to)
}

fn post_at(message: u32, at: Point) -> bool {
    let Some(window) = window() else {
        return false;
    };
    let x = WPARAM(at.x.round() as i32 as usize);
    let y = LPARAM(at.y.round() as i32 as isize);
    unsafe { PostMessageW(Some(window), message, x, y) }.is_ok()
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SETCURSOR => {
            // Windows asks the window under the cursor which cursor to show.
            unsafe { SetCursor(None) };
            LRESULT(1)
        }
        SHOW_AT => {
            let (x, y) = (wparam.0 as i32, lparam.0 as i32);
            let _ = unsafe {
                SetWindowPos(
                    window,
                    Some(HWND_TOPMOST),
                    x - SIZE / 2,
                    y - SIZE / 2,
                    SIZE,
                    SIZE,
                    SWP_SHOWWINDOW | SWP_NOACTIVATE,
                )
            };
            let _ = unsafe { SetCursorPos(x, y) };
            // Makes Windows look at what's under the cursor again. Real mouse
            // movement won't, since it's being held back while input goes elsewhere.
            // The input hook ignores this because it's injected.
            inject::nudge_nowhere();
            LRESULT(0)
        }
        HIDE | HIDE_AND_MOVE => {
            let _ = unsafe {
                SetWindowPos(
                    window,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                )
            };
            if message == HIDE_AND_MOVE {
                let (x, y) = (wparam.0 as i32, lparam.0 as i32);
                let _ = unsafe { SetCursorPos(x, y) };
            }
            // So whatever window is under the cursor now sets the cursor shape,
            // rather than leaving it hidden until the mouse next moves.
            inject::nudge_nowhere();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}
