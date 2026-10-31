//! What's open on a computer and what's playing there, as its other devices hear about it.

use serde::{Deserialize, Serialize};

use crate::geometry::Rect;

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
/// A monitor's name is a device path, which is long but not this long.
const MAX_MONITOR_TEXT: usize = 1024;

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

/// Something to do with an app's windows, here or from another device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppAction {
    /// Brings it to the front, restoring it if it's minimized. That's one of its windows on
    /// this monitor if there is one, and otherwise the one nearest the front.
    BringUp { monitor: Option<String> },
    /// Moves its windows on one monitor to another.
    MoveTo { from: String, to: String },
}

impl AppAction {
    /// Text from another device that's far too long for a monitor's name means nothing.
    pub fn makes_sense(&self) -> bool {
        let fits = |text: &String| text.chars().count() <= MAX_MONITOR_TEXT;
        match self {
            Self::BringUp { monitor } => monitor.as_ref().is_none_or(fits),
            Self::MoveTo { from, to } => fits(from) && fits(to) && from != to,
        }
    }
}

/// Where a window goes when it's moved from one screen's usable area to another's. It keeps
/// its size if there's room for it, and its place in proportion to the room there is to move
/// about in, so a window against the right edge stays against the right edge. It always ends up
/// fully on the new screen.
pub fn place_on_other_screen(window: Rect, from: Rect, to: Rect) -> Rect {
    let width = window.width.min(to.width);
    let height = window.height.min(to.height);
    let along = |start: f64,
                 size: f64,
                 from_start: f64,
                 from_size: f64,
                 to_start: f64,
                 to_size: f64,
                 new_size: f64| {
        let room = from_size - size;
        let fraction = if room > 0.0 {
            ((start - from_start) / room).clamp(0.0, 1.0)
        } else {
            0.0
        };
        to_start + (fraction * (to_size - new_size)).round()
    };
    Rect::new(
        along(
            window.x,
            window.width,
            from.x,
            from.width,
            to.x,
            to.width,
            width,
        ),
        along(
            window.y,
            window.height,
            from.y,
            from.height,
            to.y,
            to.height,
            height,
        ),
        width,
        height,
    )
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

    const LEFT: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1040.0,
    };
    const RIGHT: Rect = Rect {
        x: 1920.0,
        y: -200.0,
        width: 2560.0,
        height: 1400.0,
    };

    #[test]
    fn a_window_keeps_its_size_and_its_place_on_the_new_screen() {
        // In the top left corner, and it stays there.
        let moved = place_on_other_screen(Rect::new(0.0, 0.0, 800.0, 600.0), LEFT, RIGHT);
        assert_eq!(moved, Rect::new(1920.0, -200.0, 800.0, 600.0));
        // Against the bottom right, and it stays there too, however big the new screen is.
        let moved = place_on_other_screen(Rect::new(1120.0, 440.0, 800.0, 600.0), LEFT, RIGHT);
        assert_eq!(moved.right(), RIGHT.right());
        assert_eq!(moved.bottom(), RIGHT.bottom());
        // Half way.
        let moved = place_on_other_screen(Rect::new(560.0, 220.0, 800.0, 600.0), LEFT, RIGHT);
        assert_eq!(moved.x, 1920.0 + 880.0);
        assert_eq!(moved.y, -200.0 + 400.0);
    }

    #[test]
    fn a_window_too_big_for_the_new_screen_is_made_to_fit() {
        let moved = place_on_other_screen(Rect::new(1920.0, -200.0, 2400.0, 1300.0), RIGHT, LEFT);
        assert_eq!(moved, Rect::new(0.0, 0.0, 1920.0, 1040.0));
    }

    #[test]
    fn nonsense_from_another_device_is_noticed() {
        let long = "m".repeat(MAX_MONITOR_TEXT + 1);
        assert!(
            !AppAction::BringUp {
                monitor: Some(long.clone())
            }
            .makes_sense()
        );
        assert!(
            !AppAction::MoveTo {
                from: long,
                to: "a".into()
            }
            .makes_sense()
        );
        assert!(
            !AppAction::MoveTo {
                from: "a".into(),
                to: "a".into()
            }
            .makes_sense()
        );
        assert!(AppAction::BringUp { monitor: None }.makes_sense());
        assert!(
            AppAction::MoveTo {
                from: "a".into(),
                to: "b".into()
            }
            .makes_sense()
        );
    }

    proptest::proptest! {
        #[test]
        fn a_moved_window_is_always_fully_on_the_new_screen(
            x in -3000.0f64..3000.0,
            y in -3000.0f64..3000.0,
            width in 1.0f64..4000.0,
            height in 1.0f64..4000.0,
            to_width in 200.0f64..4000.0,
            to_height in 200.0f64..4000.0,
        ) {
            let to = Rect::new(-1000.0, 500.0, to_width.round(), to_height.round());
            let moved = place_on_other_screen(Rect::new(x, y, width, height), LEFT, to);
            proptest::prop_assert!(moved.x >= to.x - 0.5);
            proptest::prop_assert!(moved.y >= to.y - 0.5);
            proptest::prop_assert!(moved.right() <= to.right() + 0.5);
            proptest::prop_assert!(moved.bottom() <= to.bottom() + 0.5);
        }
    }
}
