//! What's open on a computer and what's playing there, as its other devices hear about it.

use serde::{Deserialize, Serialize};

/// A device listing more apps than this is misbehaving. The rest are left out.
pub const MAX_APPS: usize = 64;
/// How much of a name or identifier from another device is kept, in characters.
pub const MAX_APP_TEXT: usize = 80;
/// How many monitors of an app from another device are kept.
pub const MAX_APP_MONITORS: usize = 16;
/// A picture of an app larger than this isn't the small icon it should be.
pub const MAX_ICON_BYTES: usize = 32 * 1024;
/// How many pictures of apps to keep for one device.
pub const MAX_ICONS: usize = 128;

/// An app with a window open, or something playing, on a computer.
///
/// This goes between devices, and that encoding goes by position: see `hk_proto` before
/// changing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningApp {
    /// What identifies it, like "spotify". The same however many windows it has, and
    /// whatever language the computer is in.
    pub id: String,
    /// What to call it, like "Spotify".
    pub name: String,
    /// The monitors it has windows on, frontmost first. An app with none, such as one
    /// playing from the tray, is shown on the main monitor.
    pub monitors: Vec<String>,
    /// All of its windows are minimized.
    pub minimized: bool,
    /// Set if it can be played and paused from elsewhere.
    pub media: Option<MediaState>,
}

impl RunningApp {
    /// Whether to show this app on a monitor.
    pub fn is_on(&self, monitor: &str, primary: bool) -> bool {
        if self.monitors.is_empty() {
            primary
        } else {
            self.monitors.iter().any(|on| on == monitor)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaState {
    /// Whether it's playing right now. Some apps don't say.
    pub playing: Option<bool>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaCommand {
    PlayPause,
    /// The previous track, or a little way back if there isn't one.
    Back,
    /// The next track, or a little way forward if there isn't one.
    Forward,
}

/// Makes a list from another device safe to keep and show: nothing nameless, and nothing
/// larger than it should be.
pub fn tidied(mut apps: Vec<RunningApp>) -> Vec<RunningApp> {
    apps.retain(|app| !app.id.is_empty());
    apps.truncate(MAX_APPS);
    for app in &mut apps {
        app.id = shortened(&app.id);
        app.name = shortened(&app.name);
        app.monitors.truncate(MAX_APP_MONITORS);
    }
    apps
}

fn shortened(text: &str) -> String {
    text.chars().take(MAX_APP_TEXT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, monitors: &[&str]) -> RunningApp {
        RunningApp {
            id: id.into(),
            name: id.into(),
            monitors: monitors.iter().map(ToString::to_string).collect(),
            minimized: false,
            media: None,
        }
    }

    #[test]
    fn an_app_is_shown_on_the_monitors_it_has_windows_on() {
        let app = app("code", &["left", "right"]);
        assert!(app.is_on("left", false));
        assert!(app.is_on("right", true));
        assert!(!app.is_on("other", true));
    }

    #[test]
    fn an_app_with_no_window_is_shown_on_the_main_monitor() {
        let app = app("spotify", &[]);
        assert!(app.is_on("anything", true));
        assert!(!app.is_on("anything", false));
    }

    #[test]
    fn a_list_from_another_device_is_cut_down_to_size() {
        let long = "é".repeat(MAX_APP_TEXT * 3);
        let mut too_many: Vec<RunningApp> =
            (0..MAX_APPS * 2).map(|_| app("many", &["m"])).collect();
        too_many[0].id.clone_from(&long);
        too_many[0].name.clone_from(&long);
        too_many[0].monitors = vec!["m".into(); MAX_APP_MONITORS * 2];

        let tidy = tidied(too_many);
        assert_eq!(tidy.len(), MAX_APPS);
        assert_eq!(tidy[0].id.chars().count(), MAX_APP_TEXT);
        assert_eq!(tidy[0].name.chars().count(), MAX_APP_TEXT);
        assert_eq!(tidy[0].monitors.len(), MAX_APP_MONITORS);
    }

    #[test]
    fn nothing_nameless_is_kept() {
        assert_eq!(tidied(vec![app("", &[])]), vec![]);
    }
}
