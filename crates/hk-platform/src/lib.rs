//! Operating-system access for Hackerman: the monitors, the cursor, and
//! capturing and replaying keyboard and mouse input.
//!
//! [`Platform`] is what the rest of the program uses. [`native`] gives the real
//! implementation for this operating system, and [`fake::FakePlatform`] a
//! simulated one for tests. So far only Windows has a real implementation;
//! elsewhere [`native`] returns [`Error::Unsupported`].

pub mod fake;

#[cfg_attr(windows, path = "windows/mod.rs")]
#[cfg_attr(not(windows), path = "unsupported.rs")]
mod imp;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use hk_core::apps::{MediaCommand, RunningApp};
use hk_core::clipboard::{ClipboardData, SharedFile};
use hk_core::geometry::Point;
use hk_core::input::InputEvent;
use hk_core::layout::Monitor;
use hk_core::switcher::LocalEvent;

pub use imp::{NativePlatform, native};

#[cfg(windows)]
pub use imp::{native_for_lock_screen, session};

/// For tests that hand files to Windows' paste code without going through the clipboard.
#[cfg(windows)]
#[doc(hidden)]
pub use imp::data_object;

/// Called for each captured input event. Returns `true` to hide the event from
/// this machine. It runs while the machine's input waits, so it must be quick.
pub type InputHandler = Box<dyn FnMut(LocalEvent) -> bool + Send>;

pub trait Platform: Clone + Send + Sync + 'static {
    /// This machine's monitors right now.
    fn monitors(&self) -> Vec<Monitor>;

    /// The make and model of this computer, if it says, like "Dell Latitude 7420".
    fn model(&self) -> Option<String>;

    /// The user's Desktop folder, wherever Windows or OneDrive really keeps it.
    fn desktop_folder(&self) -> Option<PathBuf>;

    /// Whether this agent should stop and leave this computer to the other one: the user's
    /// agent when the screen locks, if the lock-screen helper is set up, and the helper when
    /// the screen is unlocked.
    fn should_step_aside(&self) -> bool {
        false
    }

    /// Where the cursor waits while input goes to another machine.
    fn park_point(&self) -> Point;

    /// Moves the cursor out of the way and hides it, while input goes elsewhere.
    fn park_cursor(&self, at: Point);

    /// Shows the cursor again, at this position.
    fn restore_cursor(&self, to: Point);

    /// Replays input as if it came from this machine's own devices. This doesn't wait for
    /// that to happen.
    fn inject(&self, event: &InputEvent);

    /// Waits a moment for what's been given to [`Platform::inject`] to be replayed, so that
    /// a program about to stop doesn't leave keys held down.
    fn finish_injecting(&self) {}

    /// Starts capturing all keyboard and mouse input, until the returned value
    /// is dropped.
    fn start_capture(&self, handler: InputHandler) -> Result<Capture, Error>;

    /// Whether a full-screen app or game is in front.
    fn fullscreen_app_in_front(&self) -> bool;

    /// Calls `on_change` whenever something new is copied to the clipboard here,
    /// until the returned value is dropped. Content that apps mark as private, and
    /// what [`Platform::write_clipboard`] itself writes, don't count.
    fn watch_clipboard(&self, on_change: Box<dyn FnMut() + Send>) -> Result<Capture, Error>;

    /// What's on the clipboard. May wait on other programs, so don't call it where
    /// that would hold things up.
    fn read_clipboard(&self) -> Result<ClipboardData, Error>;

    /// Replaces what's on the clipboard. May wait on other programs.
    fn write_clipboard(&self, data: &ClipboardData) -> Result<(), Error>;

    /// The files and folders copied to the clipboard here, if that's what it holds.
    /// May wait on other programs.
    fn clipboard_files(&self) -> Result<Vec<PathBuf>, Error>;

    /// Replaces what's on the clipboard with files that are elsewhere. Pasting them
    /// reads their content from `source`, and until then none of it moves. May wait on
    /// other programs.
    fn write_clipboard_files(
        &self,
        files: Vec<SharedFile>,
        source: Arc<dyn FileSource>,
    ) -> Result<(), Error>;

    /// The apps open here, frontmost first, and whatever is playing. May wait on other
    /// programs. Window titles stay out of this: they can say what someone is working on.
    fn running_apps(&self) -> Vec<RunningApp> {
        Vec::new()
    }

    /// A small picture of an app from [`Platform::running_apps`], as a PNG. May wait on
    /// other programs.
    fn app_icon(&self, _app: &str) -> Option<Vec<u8>> {
        None
    }

    /// Asks an app from [`Platform::running_apps`] to do this. May wait on it.
    fn media_command(&self, _app: &str, _command: MediaCommand) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// Other programs running here that share the keyboard and mouse too, which can get in
    /// each other's way.
    fn rival_programs(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Where the content of files on the clipboard comes from when they're elsewhere.
pub trait FileSource: Send + Sync {
    /// Starts reading the file at this position in the list, this many bytes in.
    /// Reading can wait a long time, so it must not be done where that holds things up.
    fn open(&self, index: usize, offset: u64) -> Box<dyn Read + Send>;
}

/// Keeps input capture running. Dropping it stops the capture.
pub struct Capture(#[allow(dead_code)] Box<dyn Send>);

impl Capture {
    pub(crate) fn new(guard: impl Send + 'static) -> Self {
        Self(Box::new(guard))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Hackerman doesn't support this operating system yet")]
    Unsupported,
    #[error("clipboard: {0}")]
    Clipboard(String),
    #[error("installing the input hooks failed: {0}")]
    Hook(String),
    #[error("media controls: {0}")]
    Media(String),
}
