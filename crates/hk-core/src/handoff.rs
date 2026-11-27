//! Handing an app off to another computer: what it has open there, opened here (ADR 1).

use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

/// The browsers whose address bar can be read, as app ids. Only these are started by name.
pub const BROWSERS: [&str; 5] = ["msedge", "chrome", "brave", "vivaldi", "opera"];
/// The longest address or path taken from another device, in characters.
const MAX_TEXT: usize = 4096;

/// Something to open on the computer an app was handed off to.
///
/// This goes between devices, and that encoding goes by position: see `hk_proto` before
/// changing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Opening {
    /// A web page, and the browser it was in, like "msedge".
    Page { address: String, browser: String },
    /// A folder open in VS Code.
    Project { folder: String },
    /// A folder open in File Explorer.
    Folder { folder: String },
}

/// What an app has open, as the computer it runs on finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Open(Opening),
    /// A file, which goes to the other computer as a copy.
    File(PathBuf),
}

impl Opening {
    /// A page from an address bar, which may leave out `https://`.
    pub fn page(address: &str, browser: &str) -> Result<Self, String> {
        let address = address.trim();
        let address = if address.contains("://") {
            address.to_string()
        } else {
            format!("https://{address}")
        };
        let page = Self::Page {
            address,
            browser: browser.to_string(),
        };
        if page.is_safe() {
            Ok(page)
        } else {
            Err("only web pages can be handed off".into())
        }
    }

    /// Whether this can be opened as it is, coming from another device. It's passed to a
    /// program as an argument, so it mustn't be able to add arguments of its own.
    pub fn is_safe(&self) -> bool {
        let plain = |text: &str| {
            !text.is_empty()
                && text.chars().count() <= MAX_TEXT
                && !text.chars().any(|c| c.is_control() || c == '"')
        };
        match self {
            Self::Page { address, .. } => {
                let lower = address.to_ascii_lowercase();
                plain(address)
                    && !address.contains(char::is_whitespace)
                    && (lower.starts_with("https://") || lower.starts_with("http://"))
            }
            Self::Project { folder } | Self::Folder { folder } => {
                plain(folder) && on_a_drive(Path::new(folder))
            }
        }
    }

    /// The browser to start by name, if it's one that can be.
    pub fn browser(&self) -> Option<&str> {
        match self {
            Self::Page { browser, .. } => BROWSERS.contains(&browser.as_str()).then_some(browser),
            _ => None,
        }
    }
}

/// A full path on one of this computer's drives, like `C:\code`: not a share on another
/// computer, which Windows would sign in to.
fn on_a_drive(path: &Path) -> bool {
    let mut parts = path.components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Prefix(prefix)), Some(Component::RootDir))
            if matches!(prefix.kind(), Prefix::Disk(_))
    ) && !path.components().any(|part| part == Component::ParentDir)
}

/// Where a folder from another computer may be here: at the same path, or at the same place
/// in this user's own folder, since user names differ between computers.
pub fn places_to_look(folder: &Path, home: &Path) -> Vec<PathBuf> {
    let mut places = vec![folder.to_path_buf()];
    let parts: Vec<Component> = folder.components().collect();
    // C:\ Users\ name\ ...
    if parts.len() >= 4
        && let Component::Normal(users) = parts[2]
        && users.eq_ignore_ascii_case("users")
    {
        let mine = parts[4..]
            .iter()
            .fold(home.to_path_buf(), |path, part| path.join(part));
        if mine != places[0] {
            places.push(mine);
        }
    }
    places
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_bar_without_https_still_gives_a_page() {
        assert_eq!(
            Opening::page("github.com/medAmarFilali", "chrome"),
            Ok(Opening::Page {
                address: "https://github.com/medAmarFilali".into(),
                browser: "chrome".into()
            })
        );
        assert!(Opening::page("http://localhost:3000/", "msedge").is_ok());
    }

    #[test]
    fn only_web_pages_are_opened() {
        for address in ["edge://settings", "file:///C:/Windows/System32/calc.exe"] {
            assert!(Opening::page(address, "msedge").is_err(), "{address}");
        }
        // Without "://" it's taken as a site's name, so it can only ever be a web address.
        for address in ["javascript:alert(1)", "ms-settings:privacy"] {
            let Ok(Opening::Page { address, .. }) = Opening::page(address, "msedge") else {
                panic!("{address}");
            };
            assert!(address.starts_with("https://"), "{address}");
        }
    }

    /// The address is given to the browser as an argument, and Chromium's arguments can run
    /// programs.
    #[test]
    fn an_address_cant_add_arguments() {
        for address in [
            "https://a.com\" --gpu-launcher=calc",
            "https://a.com --gpu-launcher=calc",
            "https://a.com\n--x",
        ] {
            let page = Opening::Page {
                address: address.into(),
                browser: "chrome".into(),
            };
            assert!(!page.is_safe(), "{address:?}");
        }
    }

    #[test]
    fn only_known_browsers_are_started_by_name() {
        let page = |browser: &str| Opening::Page {
            address: "https://a.com".into(),
            browser: browser.into(),
        };
        assert_eq!(page("msedge").browser(), Some("msedge"));
        assert_eq!(page("powershell").browser(), None);
    }

    // Drives and `\\server` shares only mean something to Windows' paths.
    #[cfg(windows)]
    #[test]
    fn only_folders_on_this_computers_drives_are_opened() {
        let folder = |path: &str| Opening::Folder {
            folder: path.into(),
        };
        assert!(folder(r"C:\Users\sam\Documents\code\hackerman").is_safe());
        assert!(!folder(r"\\server\share\x").is_safe());
        assert!(!folder(r"code\hackerman").is_safe());
        assert!(!folder(r"C:\Users\..\Windows").is_safe());
        assert!(!folder(r#"C:\a" --x"#).is_safe());
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_in_another_users_home_is_looked_for_in_mine() {
        let home = Path::new(r"C:\Users\alex");
        assert_eq!(
            places_to_look(Path::new(r"C:\Users\sam\Documents\code\x"), home),
            [
                PathBuf::from(r"C:\Users\sam\Documents\code\x"),
                PathBuf::from(r"C:\Users\alex\Documents\code\x"),
            ]
        );
        assert_eq!(
            places_to_look(Path::new(r"D:\work\x"), home),
            [PathBuf::from(r"D:\work\x")]
        );
        assert_eq!(
            places_to_look(Path::new(r"C:\Users\alex\x"), home),
            [PathBuf::from(r"C:\Users\alex\x")]
        );
    }
}
