//! Stand-in for operating systems Hackerman doesn't support yet. It can never
//! be constructed.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use hk_core::clipboard::{ClipboardData, SharedFile};
use hk_core::geometry::Point;
use hk_core::input::InputEvent;
use hk_core::layout::Monitor;

use crate::{Capture, Error, FileSource, InputHandler, Platform};

#[derive(Debug, Clone)]
pub struct NativePlatform(Infallible);

pub fn native() -> Result<NativePlatform, Error> {
    Err(Error::Unsupported)
}

impl Platform for NativePlatform {
    fn monitors(&self) -> Vec<Monitor> {
        match self.0 {}
    }

    fn model(&self) -> Option<String> {
        match self.0 {}
    }

    fn desktop_folder(&self) -> Option<PathBuf> {
        match self.0 {}
    }

    fn park_point(&self) -> Point {
        match self.0 {}
    }

    fn park_cursor(&self, _at: Point) {
        match self.0 {}
    }

    fn restore_cursor(&self, _to: Point) {
        match self.0 {}
    }

    fn inject(&self, _event: &InputEvent) {
        match self.0 {}
    }

    fn start_capture(&self, _handler: InputHandler) -> Result<Capture, Error> {
        match self.0 {}
    }

    fn fullscreen_app_in_front(&self) -> bool {
        match self.0 {}
    }

    fn watch_clipboard(&self, _on_change: Box<dyn FnMut() + Send>) -> Result<Capture, Error> {
        match self.0 {}
    }

    fn read_clipboard(&self) -> Result<ClipboardData, Error> {
        match self.0 {}
    }

    fn write_clipboard(&self, _data: &ClipboardData) -> Result<(), Error> {
        match self.0 {}
    }

    fn clipboard_files(&self) -> Result<Vec<PathBuf>, Error> {
        match self.0 {}
    }

    fn write_clipboard_files(
        &self,
        _files: Vec<SharedFile>,
        _source: Arc<dyn FileSource>,
    ) -> Result<(), Error> {
        match self.0 {}
    }
}
