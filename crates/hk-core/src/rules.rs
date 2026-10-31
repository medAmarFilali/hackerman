//! Settings for how this device behaves when sharing input.

use serde::{Deserialize, Serialize};

use crate::input::{KeyCode, optional_key_by_name};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rules {
    /// Share the clipboard with other devices.
    pub clipboard: bool,
    /// Keyboard shortcuts for locking the cursor and jumping between devices.
    /// Ctrl+Alt+Home always works, since it's the way back from anything.
    pub shortcuts: bool,
    /// Don't let the cursor leave while a full-screen app or game is in front.
    pub stay_in_fullscreen: bool,
    /// How long the cursor must push against an edge before it crosses, so that
    /// reaching for something near the edge of the screen doesn't cross by accident.
    pub edge_delay_ms: u32,
    /// A key that must be held for the cursor to cross to another device.
    pub cross_modifier: CrossModifier,
    /// The cursor doesn't cross this close to a corner of the screen, in pixels, so
    /// that reaching for the Start button or a window's close button stays here.
    pub corner_guard: u32,
    /// What happens to files another device sends here.
    pub receive_files: ReceiveFiles,
    /// Where those files go. Without this, they go on the Desktop.
    pub drop_folder: Option<String>,
    /// What to do when a file with that name is already there.
    pub name_clash: NameClash,
    /// Show a notification when files from another device have arrived, since that
    /// can happen with the window closed.
    pub notify_files: bool,
    /// Tell other devices the names of the apps open here.
    pub share_apps: bool,
    /// Let other devices play, pause and skip what's playing here.
    pub media_control: bool,
    /// Let other devices bring apps here to the front, and move them between the screens here.
    pub app_control: bool,
    /// Keyboard shortcuts for what's playing, here or on another device.
    pub media_shortcuts: MediaShortcuts,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            clipboard: true,
            shortcuts: true,
            stay_in_fullscreen: false,
            edge_delay_ms: 0,
            cross_modifier: CrossModifier::default(),
            corner_guard: 0,
            receive_files: ReceiveFiles::default(),
            drop_folder: None,
            name_clash: NameClash::default(),
            notify_files: true,
            share_apps: true,
            media_control: true,
            app_control: true,
            media_shortcuts: MediaShortcuts::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaShortcuts {
    pub enabled: bool,
    pub play_pause: Shortcut,
    pub back: Shortcut,
    pub forward: Shortcut,
}

impl Default for MediaShortcuts {
    /// Away from the arrow keys, which are Ctrl+Alt shortcuts of their own in VS Code.
    fn default() -> Self {
        Self {
            enabled: true,
            play_pause: Shortcut::ctrl_alt(KeyCode::Space),
            back: Shortcut::ctrl_alt(KeyCode::PageUp),
            forward: Shortcut::ctrl_alt(KeyCode::PageDown),
        }
    }
}

/// A key and the modifiers held with it. With no key, it's turned off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Shortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// Which physical key, whatever the keyboard layout. A settings file has no way to
    /// write "none", so one with no key is written as a shortcut without this.
    #[serde(with = "optional_key_by_name", skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyCode>,
}

impl Shortcut {
    const fn ctrl_alt(key: KeyCode) -> Self {
        Self {
            ctrl: true,
            alt: true,
            shift: false,
            meta: false,
            key: Some(key),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CrossModifier {
    #[default]
    None,
    Ctrl,
    Shift,
    Alt,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReceiveFiles {
    /// Asks about a device until the user says to always allow it. The one version with
    /// `always` had it as the default, so a settings file saying that rarely means anyone
    /// chose it, and being asked once is the safer way to be wrong.
    #[default]
    #[serde(alias = "always")]
    AskFirst,
    /// Saved straight away, from any paired device.
    AnyDevice,
    /// Saved once the user says so, every time.
    Ask,
    /// Turned away.
    Never,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NameClash {
    /// The new one gets a new name, like `report (1).pdf`.
    #[default]
    KeepBoth,
    Replace,
    /// The one already there stays, and the new one isn't saved.
    Skip,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file from before these existed has to keep working.
    #[test]
    fn rules_saved_by_an_older_version_still_load() {
        let older =
            r#"{"clipboard":false,"shortcuts":true,"stayInFullscreen":true,"edgeDelayMs":300}"#;
        let rules: Rules = serde_json::from_str(older).unwrap();
        assert!(!rules.clipboard);
        assert!(rules.stay_in_fullscreen);
        assert_eq!(rules.edge_delay_ms, 300);
        assert_eq!(rules.cross_modifier, CrossModifier::None);
        assert_eq!(rules.corner_guard, 0);
        assert_eq!(rules.receive_files, ReceiveFiles::AskFirst);
        assert_eq!(rules.drop_folder, None);
        assert_eq!(rules.name_clash, NameClash::KeepBoth);
        assert!(rules.notify_files);
        assert!(rules.share_apps);
        assert!(rules.media_control);
        assert!(rules.app_control);
        assert_eq!(rules.media_shortcuts, MediaShortcuts::default());
    }

    #[test]
    fn a_media_shortcut_turned_off_stays_off_in_a_settings_file() {
        let mut rules = Rules::default();
        rules.media_shortcuts.back = Shortcut::default();
        rules.media_shortcuts.forward = Shortcut {
            shift: true,
            key: Some(KeyCode::F13),
            ..Shortcut::default()
        };

        let written = toml::to_string(&rules).unwrap();
        let read: Rules = toml::from_str(&written).unwrap();
        assert_eq!(read, rules, "{written}");
        assert_eq!(read.media_shortcuts.back.key, None);
    }

    #[test]
    fn the_app_sees_shortcuts_by_the_names_of_their_keys() {
        let json = serde_json::to_value(MediaShortcuts::default()).unwrap();
        assert_eq!(json["playPause"]["key"], "Space");
        assert_eq!(json["playPause"]["ctrl"], true);
        assert_eq!(json["forward"]["key"], "PageDown");

        let read: Shortcut =
            serde_json::from_str(r#"{"meta":true,"key":"MediaPlayPause"}"#).unwrap();
        assert!(read.meta);
        assert_eq!(read.key, Some(KeyCode::MediaPlayPause));
    }

    #[test]
    fn a_key_this_version_has_never_heard_of_turns_the_shortcut_off() {
        let read: Shortcut =
            serde_json::from_str(r#"{"ctrl":true,"key":"KeyOfTheFuture"}"#).unwrap();
        assert_eq!(read.key, None);
    }

    #[test]
    fn the_old_default_for_receiving_files_now_asks_first() {
        let rules: Rules = serde_json::from_str(r#"{"receiveFiles":"always"}"#).unwrap();
        assert_eq!(rules.receive_files, ReceiveFiles::AskFirst);
        for (text, wanted) in [
            ("ask", ReceiveFiles::Ask),
            ("never", ReceiveFiles::Never),
            ("anyDevice", ReceiveFiles::AnyDevice),
            ("askFirst", ReceiveFiles::AskFirst),
        ] {
            let rules: Rules =
                serde_json::from_str(&format!(r#"{{"receiveFiles":"{text}"}}"#)).unwrap();
            assert_eq!(rules.receive_files, wanted);
        }
    }

    #[test]
    fn the_app_sees_these_by_their_own_names() {
        let rules = Rules {
            cross_modifier: CrossModifier::Ctrl,
            receive_files: ReceiveFiles::Ask,
            name_clash: NameClash::KeepBoth,
            drop_folder: Some("D:\\Incoming".into()),
            corner_guard: 50,
            ..Rules::default()
        };
        let json = serde_json::to_value(&rules).unwrap();
        assert_eq!(json["crossModifier"], "ctrl");
        assert_eq!(json["receiveFiles"], "ask");
        assert_eq!(json["nameClash"], "keepBoth");
        assert_eq!(json["dropFolder"], "D:\\Incoming");
        assert_eq!(json["cornerGuard"], 50);
    }
}
