//! A simulated machine, for testing everything above the operating system.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_core::apps::{AppAction, MediaCommand, RunningApp};
use hk_core::clipboard::{ClipboardData, SharedFile};
use hk_core::geometry::{Point, Rect};
use hk_core::input::InputEvent;
use hk_core::layout::Monitor;
use hk_core::screens::ScreenImage;
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
    apps: Vec<RunningApp>,
    icons: Vec<(String, Vec<u8>)>,
    icons_asked_for: Vec<String>,
    media_commands: Vec<(String, MediaCommand)>,
    app_actions: Vec<(String, AppAction)>,
    rivals: Vec<String>,
    /// What each monitor is showing, if a test has said. Others show a test pattern.
    screen_pictures: Vec<(String, ScreenImage)>,
    screens_locked: bool,
    /// Which monitors have had their pictures taken so far, and whether with the cursor.
    screen_captures: Vec<(String, bool)>,
    /// Makes each picture differ from the one before, like a screen that's in use.
    screens_moving: bool,
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

    /// Simulates these apps being open.
    pub fn set_running_apps(&self, apps: Vec<RunningApp>) {
        self.state().apps = apps;
    }

    pub fn set_app_icon(&self, app: &str, png: Vec<u8>) {
        self.state().icons.push((app.to_string(), png));
    }

    /// Which apps have had their pictures looked for, in order.
    pub fn icons_asked_for(&self) -> Vec<String> {
        self.state().icons_asked_for.clone()
    }

    /// What apps here have been asked to do so far.
    pub fn media_commands(&self) -> Vec<(String, MediaCommand)> {
        self.state().media_commands.clone()
    }

    /// What's been done with apps' windows here so far.
    pub fn app_actions(&self) -> Vec<(String, AppAction)> {
        self.state().app_actions.clone()
    }

    pub fn set_rival_programs(&self, rivals: Vec<String>) {
        self.state().rivals = rivals;
    }

    /// Simulates this picture being what's on a monitor.
    pub fn set_screen_picture(&self, monitor: &str, image: ScreenImage) {
        let mut state = self.state();
        state.screen_pictures.retain(|(id, _)| id != monitor);
        state.screen_pictures.push((monitor.to_string(), image));
    }

    /// Simulates the screen locking, so that no pictures can be taken.
    pub fn set_screens_locked(&self, locked: bool) {
        self.state().screens_locked = locked;
    }

    /// Makes each picture of a screen differ from the one before.
    pub fn set_screens_moving(&self, moving: bool) {
        self.state().screens_moving = moving;
    }

    /// Which monitors have had their pictures taken so far, and whether with the cursor.
    pub fn screen_captures(&self) -> Vec<(String, bool)> {
        self.state().screen_captures.clone()
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

    fn running_apps(&self) -> Vec<RunningApp> {
        self.state().apps.clone()
    }

    fn app_icon(&self, app: &str) -> Option<Vec<u8>> {
        let mut state = self.state();
        state.icons_asked_for.push(app.to_string());
        let found = state.icons.iter().find(|(id, _)| id == app);
        found.map(|(_, png)| png.clone())
    }

    fn media_command(&self, app: &str, command: MediaCommand) -> Result<(), Error> {
        self.state().media_commands.push((app.to_string(), command));
        Ok(())
    }

    fn app_action(&self, app: &str, action: &AppAction) -> Result<(), Error> {
        let mut state = self.state();
        state.app_actions.push((app.to_string(), action.clone()));
        if let AppAction::MoveTo { from, to } = action
            && let Some(moving) = state.apps.iter_mut().find(|known| known.id == app)
        {
            moving.monitors.retain(|on| on != from);
            if !moving.monitors.contains(to) {
                moving.monitors.push(to.clone());
            }
        }
        Ok(())
    }

    fn rival_programs(&self) -> Vec<String> {
        self.state().rivals.clone()
    }

    fn capture_screen(&self, monitor: &Monitor, with_cursor: bool) -> Result<ScreenImage, Error> {
        let mut state = self.state();
        if state.screens_locked {
            return Err(Error::Screen("the screen is locked".to_string()));
        }
        state
            .screen_captures
            .push((monitor.id.clone(), with_cursor));
        let frame = if state.screens_moving {
            state.screen_captures.len()
        } else {
            0
        };
        Ok(state
            .screen_pictures
            .iter()
            .find(|(id, _)| *id == monitor.id)
            .map_or_else(
                || test_pattern(monitor, frame),
                |(_, picture)| picture.clone(),
            ))
    }
}

/// Something to look at for a simulated screen: bands of colour that differ from one monitor to
/// another, a grid to show up poor scaling, and a bar that moves along with `frame`.
fn test_pattern(monitor: &Monitor, frame: usize) -> ScreenImage {
    let (width, height) = (
        monitor.bounds.width.max(1.0) as usize,
        monitor.bounds.height.max(1.0) as usize,
    );
    let tint = monitor
        .id
        .bytes()
        .fold(0usize, |sum, byte| sum + byte as usize)
        * 37;
    let bar = frame * 16 % width;
    let mut bgra = vec![255; width * height * 4];
    for (y, row) in bgra.chunks_exact_mut(width * 4).enumerate() {
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let grid = x % 120 < 2 || y % 120 < 2;
            let in_bar = x >= bar && x < bar + 24;
            let (blue, green, red) = if in_bar || grid {
                (240, 240, 240)
            } else {
                (
                    ((tint + x / 8) % 200 + 30) as u8,
                    ((tint / 2 + y / 6) % 160 + 40) as u8,
                    ((tint / 3 + (x + y) / 16) % 180 + 30) as u8,
                )
            };
            pixel[0] = blue;
            pixel[1] = green;
            pixel[2] = red;
        }
    }
    ScreenImage {
        width: width as u32,
        height: height as u32,
        bgra,
    }
}
