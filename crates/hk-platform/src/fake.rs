//! A simulated machine, for testing everything above the operating system.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_core::clipboard::{ClipboardData, SharedFile};
use hk_core::geometry::{Point, Rect};
use hk_core::input::InputEvent;
use hk_core::layout::Monitor;
use hk_core::switcher::LocalEvent;

use crate::{Capture, Error, FileSource, InputHandler, Platform};

/// Keeps memory bounded when a simulated device runs for a long time.
const MAX_RECORDED: usize = 10_000;

#[derive(Default)]
struct State {
    monitors: Vec<Monitor>,
    injected: Vec<InputEvent>,
    cursor: Point,
    cursor_hidden: bool,
    should_step_aside: bool,
    clipboard: ClipboardData,
    /// Files copied here.
    copied_files: Vec<PathBuf>,
    /// Files from elsewhere, ready to be pasted.
    files_to_paste: Vec<SharedFile>,
    file_source: Option<Arc<dyn FileSource>>,
    fullscreen: bool,
    model: Option<String>,
    desktop: Option<PathBuf>,
}

type ClipboardWatcher = Box<dyn FnMut() + Send>;

/// Like the real hooks, stops input reaching the handler when capture ends.
struct CaptureGuard(Arc<Mutex<Option<InputHandler>>>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// A machine with simulated monitors. Tests feed it local input with
/// [`FakePlatform::local`] and read back what the program did to it.
#[derive(Clone)]
pub struct FakePlatform {
    state: Arc<Mutex<State>>,
    handler: Arc<Mutex<Option<InputHandler>>>,
    clipboard_watcher: Arc<Mutex<Option<ClipboardWatcher>>>,
}

impl FakePlatform {
    pub fn new(monitors: Vec<Monitor>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                monitors,
                ..State::default()
            })),
            handler: Arc::default(),
            clipboard_watcher: Arc::default(),
        }
    }

    /// One 1920x1080 monitor at 100%, or several side by side.
    pub fn with_screens(count: usize) -> Self {
        Self::new(
            (0..count)
                .map(|index| Monitor {
                    id: format!("fake-{index}"),
                    name: format!("Simulated display {}", index + 1),
                    bounds: Rect::new(index as f64 * 1920.0, 0.0, 1920.0, 1080.0),
                    scale: 1.0,
                    primary: index == 0,
                })
                .collect(),
        )
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Simulates this machine's own keyboard or mouse. Returns whether the
    /// program hid the event from the machine.
    pub fn local(&self, event: LocalEvent) -> bool {
        let mut handler = self.handler.lock().unwrap_or_else(PoisonError::into_inner);
        let swallowed = handler.as_mut().is_some_and(|handler| handler(event));
        drop(handler);
        if !swallowed && let LocalEvent::Motion { to, .. } = event {
            self.state().cursor = to;
        }
        swallowed
    }

    pub fn set_monitors(&self, monitors: Vec<Monitor>) {
        self.state().monitors = monitors;
    }

    /// Everything replayed on this machine so far.
    pub fn injected(&self) -> Vec<InputEvent> {
        self.state().injected.clone()
    }

    /// Simulates this machine's own mouse moving the cursor. Returns whether the
    /// program hid the move from the machine.
    pub fn move_cursor(&self, to: Point) -> bool {
        let from = self.cursor();
        self.local(LocalEvent::Motion { from, to })
    }

    /// Puts the cursor somewhere, as if another device or program moved it there.
    pub fn warp_for_test(&self, to: Point) {
        self.state().cursor = to;
    }

    /// Simulates the screen locking, or unlocking, with the helper set up.
    pub fn set_should_step_aside(&self, should: bool) {
        self.state().should_step_aside = should;
    }

    pub fn set_desktop_folder(&self, folder: PathBuf) {
        self.state().desktop = Some(folder);
    }

    pub fn set_model(&self, model: &str) {
        self.state().model = Some(model.to_string());
    }

    fn report_copy(&self) {
        let mut watcher = self
            .clipboard_watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(watcher) = watcher.as_mut() {
            watcher();
        }
    }

    /// Simulates the user copying something on this machine.
    pub fn copy(&self, data: ClipboardData) {
        {
            let mut state = self.state();
            state.clipboard = data;
            state.copied_files.clear();
            state.files_to_paste.clear();
            state.file_source = None;
        }
        self.report_copy();
    }

    /// Simulates the user copying files and folders on this machine.
    pub fn copy_files(&self, files: Vec<PathBuf>) {
        {
            let mut state = self.state();
            state.clipboard = ClipboardData::default();
            state.copied_files = files;
            state.files_to_paste.clear();
            state.file_source = None;
        }
        self.report_copy();
    }

    pub fn clipboard(&self) -> ClipboardData {
        self.state().clipboard.clone()
    }

    /// The files from elsewhere that pasting here would bring.
    pub fn files_to_paste(&self) -> Vec<SharedFile> {
        self.state().files_to_paste.clone()
    }

    /// Reads a file from elsewhere, as pasting it would.
    pub fn read_pasted_file(&self, index: usize, offset: u64) -> Option<Box<dyn Read + Send>> {
        let source = self.state().file_source.clone()?;
        Some(source.open(index, offset))
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        self.state().fullscreen = fullscreen;
    }

    pub fn cursor_hidden(&self) -> bool {
        self.state().cursor_hidden
    }

    pub fn cursor(&self) -> Point {
        self.state().cursor
    }
}

impl Platform for FakePlatform {
    fn monitors(&self) -> Vec<Monitor> {
        self.state().monitors.clone()
    }

    fn model(&self) -> Option<String> {
        self.state().model.clone()
    }

    fn desktop_folder(&self) -> Option<PathBuf> {
        self.state().desktop.clone()
    }

    fn should_step_aside(&self) -> bool {
        self.state().should_step_aside
    }

    fn park_point(&self) -> Point {
        Point::new(960.0, 540.0)
    }

    fn park_cursor(&self, at: Point) {
        let mut state = self.state();
        state.cursor = at;
        state.cursor_hidden = true;
    }

    fn restore_cursor(&self, to: Point) {
        let mut state = self.state();
        state.cursor = to;
        state.cursor_hidden = false;
    }

    fn inject(&self, event: &InputEvent) {
        let mut state = self.state();
        if state.injected.len() >= MAX_RECORDED {
            state.injected.clear();
        }
        state.injected.push(*event);
    }

    fn start_capture(&self, handler: InputHandler) -> Result<Capture, Error> {
        *self.handler.lock().unwrap_or_else(PoisonError::into_inner) = Some(handler);
        Ok(Capture::new(CaptureGuard(self.handler.clone())))
    }

    fn fullscreen_app_in_front(&self) -> bool {
        self.state().fullscreen
    }

    fn watch_clipboard(&self, on_change: Box<dyn FnMut() + Send>) -> Result<Capture, Error> {
        *self
            .clipboard_watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(on_change);
        Ok(Capture::new(()))
    }

    fn read_clipboard(&self) -> Result<ClipboardData, Error> {
        Ok(self.state().clipboard.clone())
    }

    fn clipboard_files(&self) -> Result<Vec<PathBuf>, Error> {
        Ok(self.state().copied_files.clone())
    }

    fn write_clipboard_files(
        &self,
        files: Vec<SharedFile>,
        source: Arc<dyn FileSource>,
    ) -> Result<(), Error> {
        let mut state = self.state();
        state.clipboard = ClipboardData::default();
        state.copied_files.clear();
        state.files_to_paste = files;
        state.file_source = Some(source);
        Ok(())
    }

    fn write_clipboard(&self, data: &ClipboardData) -> Result<(), Error> {
        let mut state = self.state();
        state.clipboard = data.clone();
        state.copied_files.clear();
        state.files_to_paste.clear();
        state.file_source = None;
        Ok(())
    }
}
