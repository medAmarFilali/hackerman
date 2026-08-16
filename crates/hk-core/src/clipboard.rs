//! What's on the clipboard, and which copy of it is the newest.
//!
//! Copying only announces a new [`ClipVersion`] to other devices. The content
//! moves when the user actually arrives at another device, so it doesn't spread
//! to computers they aren't using, and a large image isn't sent for nothing.

use serde::{Deserialize, Serialize};

use crate::device::Fingerprint;

/// Identifies one copy action. Higher versions are newer, and the owner breaks
/// ties so that all devices agree on which is newest. The fields are compared in
/// the order they're declared here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClipVersion {
    /// When it was copied, in milliseconds since 1970, or just past the newest
    /// version seen if that's later. Going by the clock means a copy still counts as
    /// the newest on a device that has restarted, or that hasn't heard of the last
    /// copy because the two devices aren't paired with each other.
    pub counter: u64,
    /// The device whose clipboard holds this content.
    pub owner: Fingerprint,
}

/// More files than this at once is refused, as a list of them has to fit in memory.
pub const MAX_SHARED_FILES: usize = 100_000;

/// What Windows' description of files being pasted has room for, in UTF-16 units.
pub const MAX_SHARED_PATH: usize = 259;

/// A file or folder on the clipboard. Its content stays on the device it's on until
/// something is pasted, so this is all that travels when the cursor arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFile {
    /// Where it goes, relative to wherever it's pasted, with `/` between folders.
    pub path: String,
    /// In bytes. Nothing beyond this much is ever read from the other device.
    pub size: u64,
    pub is_dir: bool,
    /// When it last changed, in milliseconds since 1970, if known.
    pub modified_ms: Option<u64>,
}

impl SharedFile {
    /// Whether pasting this could only ever create something inside the folder being
    /// pasted into. These paths come from another device, so a path like
    /// `../../Startup/run.exe` has to be refused rather than trusted.
    pub fn has_safe_path(&self) -> bool {
        self.path.encode_utf16().count() <= MAX_SHARED_PATH
            && self.path.split('/').all(is_safe_name)
    }
}

fn is_safe_name(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name);
    !name.is_empty()
        // Windows quietly drops these from the end, which would turn ".. " into "..".
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        && !RESERVED.iter().any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipboardData {
    pub text: Option<String>,
    /// The same content with its formatting, for apps that can use it.
    pub html: Option<String>,
    pub image: Option<ClipboardImage>,
}

impl ClipboardData {
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.image.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: Fingerprint = Fingerprint::from_bytes([1; 32]);
    const HIGH: Fingerprint = Fingerprint::from_bytes([2; 32]);

    fn version(counter: u64, owner: Fingerprint) -> ClipVersion {
        ClipVersion { counter, owner }
    }

    #[test]
    fn later_copies_are_newer_whoever_made_them() {
        assert!(version(2, LOW) > version(1, HIGH));
    }

    #[test]
    fn simultaneous_copies_have_one_agreed_winner() {
        assert!(version(1, HIGH) > version(1, LOW));
    }

    fn file(path: &str) -> SharedFile {
        SharedFile {
            path: path.into(),
            size: 0,
            is_dir: false,
            modified_ms: None,
        }
    }

    #[test]
    fn ordinary_paths_are_safe() {
        for path in [
            "report.pdf",
            "photos/2024/été à la mer.jpg",
            "a.b.c",
            "CONTRACT.docx",
            ".gitignore",
        ] {
            assert!(file(path).has_safe_path(), "{path}");
        }
    }

    #[test]
    fn paths_that_could_escape_the_folder_are_refused() {
        for path in [
            "",
            "..",
            ".",
            "../evil.exe",
            "a/../../evil.exe",
            "a/..",
            ".. ",
            "...",
            "/absolute",
            "C:/Windows/evil.exe",
            "C:evil.exe",
            "a\\..\\..\\evil.exe",
            "a//b",
            "trailing/",
            "file.txt:hidden-stream",
            "new\nline",
            "CON",
            "nul.txt",
            "folder/com1.log",
        ] {
            assert!(!file(path).has_safe_path(), "{path:?}");
        }
    }

    #[test]
    fn paths_too_long_for_windows_are_refused() {
        assert!(file(&"a".repeat(MAX_SHARED_PATH)).has_safe_path());
        assert!(!file(&"a".repeat(MAX_SHARED_PATH + 1)).has_safe_path());
    }

    #[test]
    fn empty_means_nothing_to_share() {
        assert!(ClipboardData::default().is_empty());
        let text = ClipboardData {
            text: Some("hello".into()),
            ..Default::default()
        };
        assert!(!text.is_empty());
    }
}
