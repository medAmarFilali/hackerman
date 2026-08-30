//! The Windows clipboard: watching it for changes, and reading and writing it.

use std::cell::RefCell;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use arboard::{Clipboard, ImageData};
use hk_core::clipboard::{ClipboardData, ClipboardImage};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    RegisterClipboardFormatW, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLIPBOARDUPDATE, WM_QUIT, WM_USER, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::Error;

/// Images larger than this many pixels are left alone. That's a 16K screen, and
/// each pixel takes four bytes in memory.
const MAX_IMAGE_PIXELS: u64 = 16_384 * 16_384;

// Writing to the clipboard makes Windows report a change, which mustn't look like
// the user copying something: it would announce the content straight back to the
// device it came from. Writes in progress are ignored, as is the exact state that
// a finished write left behind.
static WRITING: AtomicBool = AtomicBool::new(false);
static LAST_WRITTEN: AtomicU32 = AtomicU32::new(0);

thread_local! {
    // Window procedures receive no user data, so the watcher finds it here.
    static ON_CHANGE: RefCell<Option<Box<dyn FnMut() + Send>>> = const { RefCell::new(None) };
}

fn clipboard_error(error: impl std::fmt::Display) -> Error {
    Error::Clipboard(error.to_string())
}

/// Password managers mark what they copy with one of these, asking programs like
/// this one to leave it alone.
///
/// `CanIncludeInClipboardHistory` could in principle say "yes, it can", but apps
/// only add it to say no, as it isn't needed otherwise.
fn is_private() -> bool {
    let markers: [PCWSTR; 3] = [
        w!("ExcludeClipboardContentFromMonitorProcessing"),
        w!("CanIncludeInClipboardHistory"),
        w!("Clipboard Viewer Ignore"),
    ];
    markers.iter().any(|&name| {
        let format = unsafe { RegisterClipboardFormatW(name) };
        format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    })
}

pub(super) fn read() -> Result<ClipboardData, Error> {
    if is_private() {
        return Ok(ClipboardData::default());
    }
    let mut clipboard = Clipboard::new().map_err(clipboard_error)?;
    // Each of these fails simply if the clipboard doesn't hold that kind of thing.
    let text = clipboard.get_text().ok();
    let html = clipboard.get().html().ok();
    // When an app offers text, an image alongside it is only a picture of that
    // text, and reading one can be slow.
    let image = if text.is_none() && html.is_none() {
        clipboard
            .get_image()
            .ok()
            .and_then(|image| encode_png(&image))
    } else {
        None
    };
    Ok(ClipboardData { text, html, image })
}

/// The files and folders copied in File Explorer and the like, if any.
pub(super) fn files() -> Result<Vec<PathBuf>, Error> {
    if is_private() {
        return Ok(Vec::new());
    }
    let mut clipboard = Clipboard::new().map_err(clipboard_error)?;
    // This fails simply if the clipboard doesn't hold files.
    Ok(clipboard.get().file_list().unwrap_or_default())
}

pub(super) fn write(data: &ClipboardData) -> Result<(), Error> {
    // Decoded first, so a bad image leaves the clipboard as it was.
    let image = match (&data.text, &data.html, &data.image) {
        (None, None, Some(image)) => Some(decode_png(image)?),
        _ => None,
    };

    let mut clipboard = Clipboard::new().map_err(clipboard_error)?;
    own_write(|| match (&data.html, &data.text, image) {
        (Some(html), text, _) => clipboard.set_html(html.as_str(), text.as_deref()),
        (None, Some(text), _) => clipboard.set_text(text.as_str()),
        (None, None, Some(image)) => clipboard.set_image(image),
        (None, None, None) => Ok(()),
    })
    .map_err(clipboard_error)
}

/// Writes to the clipboard so that it isn't mistaken for the user copying something.
pub(super) fn own_write<T>(write: impl FnOnce() -> T) -> T {
    WRITING.store(true, Ordering::SeqCst);
    let result = write();
    LAST_WRITTEN.store(unsafe { GetClipboardSequenceNumber() }, Ordering::SeqCst);
    WRITING.store(false, Ordering::SeqCst);
    result
}

/// PNG is what travels between devices: a raw 4K screenshot is 33 MB, and usually
/// far less than a tenth of that as a PNG.
fn encode_png(image: &ImageData<'_>) -> Option<ClipboardImage> {
    let (width, height) = (
        u32::try_from(image.width).ok()?,
        u32::try_from(image.height).ok()?,
    );
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&image.bytes).ok()?;
    writer.finish().ok()?;
    Some(ClipboardImage { width, height, png })
}

fn decode_png(image: &ClipboardImage) -> Result<ImageData<'static>, Error> {
    let bad = |reason: &str| Error::Clipboard(format!("bad image: {reason}"));
    if u64::from(image.width) * u64::from(image.height) > MAX_IMAGE_PIXELS {
        return Err(bad("too large"));
    }
    let decoder = png::Decoder::new(Cursor::new(&image.png));
    let mut reader = decoder.read_info().map_err(clipboard_error)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| bad("too large"))?;
    let mut buffer = vec![0; size];
    let frame = reader.next_frame(&mut buffer).map_err(clipboard_error)?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return Err(bad("unexpected pixel format"));
    }
    buffer.truncate(frame.buffer_size());
    Ok(ImageData {
        width: frame.width as usize,
        height: frame.height as usize,
        bytes: buffer.into(),
    })
}

/// Tells Windows to call back when the clipboard changes, until dropped. Reading
/// the clipboard can wait on other programs, so this has a thread of its own.
pub(super) struct Watcher {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub(super) fn start(on_change: Box<dyn FnMut() + Send>) -> Result<Self, Error> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                ON_CHANGE.with_borrow_mut(|slot| *slot = Some(on_change));
                let window = match create_listener_window() {
                    Ok(window) => window,
                    Err(error) => {
                        let _ = ready_tx.send(Err(clipboard_error(error)));
                        return;
                    }
                };
                // Create this thread's message queue before anyone posts to it.
                let mut message = MSG::default();
                let _ = unsafe { PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE) };
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));

                while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                    unsafe { DispatchMessageW(&message) };
                }

                let _ = unsafe { RemoveClipboardFormatListener(window) };
                let _ = unsafe { DestroyWindow(window) };
            })
            .map_err(clipboard_error)?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(Error::Clipboard("the watcher thread stopped".into())),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn create_listener_window() -> windows::core::Result<HWND> {
    let class_name = w!("HackermanClipboardWatcher");
    let module = unsafe { GetModuleHandleW(None) }?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // Fails harmlessly if a watcher was started before in this process.
    unsafe { RegisterClassW(&class) };

    // SAFETY: the class was registered above, and the pointers it uses are static.
    // A message-only window never appears on screen.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )
    }?;
    unsafe { AddClipboardFormatListener(window) }?;
    Ok(window)
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        let own_write = WRITING.load(Ordering::SeqCst)
            || unsafe { GetClipboardSequenceNumber() } == LAST_WRITTEN.load(Ordering::SeqCst);
        if !own_write && !is_private() {
            ON_CHANGE.with_borrow_mut(|on_change| {
                if let Some(on_change) = on_change {
                    on_change();
                }
            });
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// Encodes pixels for a test elsewhere in this module's parent.
#[cfg(test)]
pub(super) fn tests_png(width: usize, height: usize, pixels: &[u8]) -> ClipboardImage {
    encode_png(&ImageData {
        width,
        height,
        bytes: pixels.into(),
    })
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_survive_the_trip_through_png() {
        // Two pixels: opaque red, and half-transparent green.
        let bytes = vec![255, 0, 0, 255, 0, 255, 0, 128];
        let image = ImageData {
            width: 2,
            height: 1,
            bytes: bytes.clone().into(),
        };
        let encoded = encode_png(&image).unwrap();
        assert_eq!((encoded.width, encoded.height), (2, 1));
        let decoded = decode_png(&encoded).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.bytes.into_owned(), bytes);
    }

    #[test]
    fn corrupt_images_are_refused() {
        let image = ClipboardImage {
            width: 2,
            height: 1,
            png: vec![1, 2, 3, 4],
        };
        assert!(decode_png(&image).is_err());
        let huge = ClipboardImage {
            width: u32::MAX,
            height: u32::MAX,
            png: vec![],
        };
        assert!(decode_png(&huge).is_err());
    }
}
