//! Messages between the Hackerman app and its background agent, which are
//! separate processes on the same computer.
//!
//! The agent keeps running with no window open, so the app is only a view: it
//! sends [`Request`]s and the agent answers every change with a full
//! [`UiState`], in a [`FromAgent`]. Input never passes through here. These are
//! JSON text lines over a local channel that only the current user can open.

mod transport;

use hk_core::apps::{AppAction, MediaCommand, MediaState};
use hk_core::device::Fingerprint;
use hk_core::layout::Placement;
use hk_core::rules::Rules;
use serde::{Deserialize, Serialize};

pub use transport::{IpcConnection, IpcListener, connect, default_channel_name};

/// What the app asks the agent to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Request {
    /// Start pairing with a device found on the network.
    Pair {
        device: Fingerprint,
    },
    /// Start pairing with a device at an address the user typed.
    PairAddress {
        address: String,
    },
    /// The code the user typed, from the screen of the other device.
    SubmitPairingCode {
        code: String,
    },
    /// Cancel the pairing in progress, or clear its result.
    DismissPairing,
    Forget {
        device: Fingerprint,
    },
    /// New canvas positions for the monitors that moved. With none, this saves the
    /// arrangement as it is for the monitors there are now.
    SetPlacements {
        placements: Vec<Placement>,
    },
    Rename {
        name: String,
    },
    /// Change how this device behaves.
    SetRules {
        rules: Rules,
    },
    /// Keep the cursor on the device it's on now, or stop doing that.
    SetLocked {
        locked: bool,
    },
    /// Turn all sharing on this device on or off.
    SetEnabled {
        enabled: bool,
    },
    /// Send these files and folders to be saved on a device, which may be this one.
    SendFiles {
        device: Fingerprint,
        paths: Vec<String>,
    },
    /// Stop a transfer going on, from either end.
    CancelTransfer {
        id: String,
    },
    /// Stop showing a transfer that's over.
    DismissTransfer {
        id: String,
    },
    /// The answer to [`UiState::incoming_files`].
    AnswerIncomingFiles {
        id: String,
        accept: bool,
        /// Don't ask about this device again.
        #[serde(default)]
        always: bool,
    },
    /// Whether a device can send files here without asking.
    SetFilesAllowed {
        device: Fingerprint,
        allowed: bool,
    },
    /// Play, pause or skip what's playing in an app on a device, which may be this one.
    MediaCommand {
        device: Fingerprint,
        app: String,
        command: MediaCommand,
    },
    /// The user is choosing a new shortcut, or has stopped. Until then, shortcuts here do
    /// nothing, so that the app can see what's pressed.
    RecordingShortcut {
        active: bool,
    },
    /// Does this with an app's windows on a device, which may be this one.
    AppAction {
        device: Fingerprint,
        app: String,
        action: AppAction,
    },
    /// Whether a device can see this device's screens.
    SetScreensAllowed {
        device: Fingerprint,
        allowed: bool,
    },
    /// Asks for a picture of a screen on a device, which may be this one. The answer comes
    /// back with this `id`, as a [`ScreenPicture`].
    ScreenPicture {
        id: u64,
        #[serde(flatten)]
        ask: ScreenAsk,
    },
    /// Stop the agent.
    Shutdown,
}

/// Which picture of which screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenAsk {
    pub device: Fingerprint,
    pub monitor: String,
    /// The picture has to fit in this.
    pub max_width: u16,
    pub max_height: u16,
    /// The [`PictureAnswer::Picture::fingerprint`] of the picture already on show, or zero.
    pub known: u32,
    /// Whether to draw the cursor on it.
    pub cursor: bool,
}

/// What the agent tells the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FromAgent {
    State(Box<UiState>),
    Picture(ScreenPicture),
}

/// The answer to [`Request::ScreenPicture`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenPicture {
    pub id: u64,
    pub answer: PictureAnswer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum PictureAnswer {
    /// A new picture, ready to show.
    #[serde(rename_all = "camelCase")]
    Picture {
        url: String,
        width: u16,
        height: u16,
        fingerprint: u32,
    },
    /// It's the same as the one already on show.
    Unchanged,
    /// That device's user hasn't allowed this.
    NotAllowed,
    /// That device is running a version from before this could be done.
    NeedsUpdate,
    /// There's nothing to show right now. That device may be locked, offline or disabled.
    Unavailable,
    /// Too many are being taken at once, so ask again in a moment.
    Busy,
}

/// Everything the app shows. Sent whenever any of it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiState {
    pub me: ThisDevice,
    /// Paired devices first, then unpaired ones found on the network.
    pub devices: Vec<DeviceView>,
    pub screens: Vec<ScreenView>,
    pub pairing: Option<PairingView>,
    pub control: ControlView,
    pub rules: Rules,
    /// Whether this device shares anything at all. While it's off, it keeps its own
    /// keyboard and mouse, and accepts nothing from other devices.
    pub enabled: bool,
    /// The user asked for the cursor to stay on the device it's on now.
    pub locked: bool,
    /// The cursor is staying here because a full-screen app is in front.
    pub held_by_fullscreen: bool,
    /// Set if input can't be captured, so this computer can't control others.
    pub capture_error: Option<String>,
    /// Where files go unless the rules name another folder.
    pub desktop_folder: Option<String>,
    /// Files being sent or saved, and recently finished, oldest first.
    pub transfers: Vec<TransferView>,
    /// Files another device wants to send here, for the user to decide about.
    pub incoming_files: Option<IncomingFilesView>,
    /// Whether an arrangement is saved for exactly the screens there are now.
    pub layout_saved: bool,
    /// How many sets of screens have an arrangement saved.
    pub saved_layouts: usize,
    /// Other programs running here that share the keyboard and mouse too, like "Input
    /// Director". They can get in each other's way.
    pub rival_programs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferView {
    pub id: String,
    /// The other device. It's this device for files saved here from here.
    pub peer: Fingerprint,
    /// Whether the files are going from here to somewhere else.
    pub sending: bool,
    /// What's being sent, like "report.pdf" or "report.pdf and 2 more".
    pub label: String,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub stage: TransferStage,
    /// Where the files are going, on whichever device that is.
    pub folder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "camelCase")]
pub enum TransferStage {
    /// Going through the files, or waiting for the other device to say it wants them.
    Waiting,
    Running,
    Done,
    Failed {
        reason: String,
    },
    Cancelled,
}

impl TransferStage {
    pub fn is_over(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. } | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingFilesView {
    pub id: String,
    pub peer_name: String,
    /// What's being sent, like "report.pdf" or "report.pdf and 2 more".
    pub label: String,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisDevice {
    pub fingerprint: Fingerprint,
    pub name: String,
    /// This copy of Hackerman.
    pub version: String,
    /// The make and model, like "Lenovo ThinkPad X1 Carbon", if it says.
    pub model: Option<String>,
    /// Best first.
    pub addresses: Vec<String>,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceStatus {
    /// Paired and connected.
    Connected,
    /// Paired and connected, but its user has turned sharing off.
    Off,
    /// Paired, but not reachable right now.
    Offline,
    /// Paired, but running a different version of Hackerman.
    Incompatible,
    /// Found on the network, not paired.
    Available,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub status: DeviceStatus,
    pub model: Option<String>,
    pub addresses: Vec<String>,
    /// Connected, but running a version from before files could be shared.
    pub needs_update_for_files: bool,
    /// Connected, and able to save files sent to it. That leaves out a device that's
    /// disabled, and one running a version from before this could be done.
    pub can_receive_files: bool,
    /// The user said it can send files here without asking.
    pub files_allowed: bool,
    /// The user said it can see this device's screens.
    pub screens_allowed: bool,
    /// It has just been looking at them.
    pub watching_screens: bool,
}

/// A monitor on the canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenView {
    pub device: Fingerprint,
    pub monitor: String,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The width the user gave it on the canvas, if they resized it.
    pub custom_width: Option<f64>,
    pub pixel_width: f64,
    pub pixel_height: f64,
    pub scale: f64,
    pub primary: bool,
    pub online: bool,
    /// What's open on this screen, frontmost first.
    pub apps: Vec<AppView>,
}

/// An app on a screen on the canvas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppView {
    pub id: String,
    pub name: String,
    /// A picture of it, ready to show. Some apps have none to be found.
    pub icon: Option<String>,
    pub minimized: bool,
    /// Set if it can be played and paused from here.
    pub media: Option<MediaState>,
    /// It has a window to bring up or move. One playing from the tray doesn't.
    pub has_window: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingView {
    pub peer_name: Option<String>,
    pub stage: PairingStage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "camelCase")]
pub enum PairingStage {
    /// Reaching the other device.
    Connecting,
    /// Waiting for the user to type the code shown on the other device.
    EnterCode,
    /// The other device asked to pair: show this code for the user to type there.
    ShowCode {
        code: String,
    },
    Verifying,
    Paired,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ControlView {
    /// This computer's keyboard and mouse control this computer.
    Home,
    /// This computer's keyboard and mouse control another device.
    Controlling { device: Fingerprint },
    /// Another device's keyboard and mouse control this computer.
    ControlledBy { device: Fingerprint },
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: Fingerprint = Fingerprint::from_bytes([7; 32]);

    #[test]
    fn requests_use_the_format_the_app_sends() {
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"setPlacements","placements":[{{"device":"{DEVICE}","monitor":"m","x":1.5,"y":-2.0}}]}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::SetPlacements {
                placements: vec![Placement {
                    device: DEVICE,
                    monitor: "m".into(),
                    x: 1.5,
                    y: -2.0,
                    width: None,
                }]
            }
        );
        let request: Request =
            serde_json::from_str(r#"{"type":"submitPairingCode","code":"123456"}"#).unwrap();
        assert_eq!(
            request,
            Request::SubmitPairingCode {
                code: "123456".into()
            }
        );
    }

    #[test]
    fn saying_yes_to_files_is_only_for_this_once_unless_it_says_otherwise() {
        let request: Request =
            serde_json::from_str(r#"{"type":"answerIncomingFiles","id":"1","accept":true}"#)
                .unwrap();
        assert_eq!(
            request,
            Request::AnswerIncomingFiles {
                id: "1".into(),
                accept: true,
                always: false,
            }
        );
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"setFilesAllowed","device":"{DEVICE}","allowed":false}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::SetFilesAllowed {
                device: DEVICE,
                allowed: false,
            }
        );
    }

    #[test]
    fn rules_use_the_format_the_app_sends() {
        let request: Request = serde_json::from_str(
            r#"{"type":"setRules","rules":{"clipboard":false,"shortcuts":true,"stayInFullscreen":true,"edgeDelayMs":300}}"#,
        )
        .unwrap();
        assert_eq!(
            request,
            Request::SetRules {
                rules: Rules {
                    clipboard: false,
                    shortcuts: true,
                    stay_in_fullscreen: true,
                    edge_delay_ms: 300,
                    ..Rules::default()
                }
            }
        );
        let request: Request =
            serde_json::from_str(r#"{"type":"setLocked","locked":true}"#).unwrap();
        assert_eq!(request, Request::SetLocked { locked: true });
    }

    #[test]
    fn the_whole_state_uses_the_names_the_app_reads() {
        let state = UiState {
            me: ThisDevice {
                fingerprint: DEVICE,
                name: "desk".into(),
                version: "1.2.3".into(),
                model: Some("Dell XPS 13".into()),
                addresses: vec![],
                port: 1,
            },
            devices: vec![DeviceView {
                fingerprint: DEVICE,
                name: "other".into(),
                status: DeviceStatus::Incompatible,
                model: None,
                addresses: vec![],
                needs_update_for_files: true,
                can_receive_files: false,
                files_allowed: true,
                screens_allowed: true,
                watching_screens: true,
            }],
            screens: vec![],
            pairing: None,
            control: ControlView::Home,
            rules: Rules::default(),
            enabled: false,
            locked: true,
            held_by_fullscreen: true,
            capture_error: None,
            desktop_folder: Some("C:\\Bureau".into()),
            transfers: vec![TransferView {
                id: "1".into(),
                peer: DEVICE,
                sending: true,
                label: "report.pdf".into(),
                total_bytes: 10,
                done_bytes: 5,
                stage: TransferStage::Failed {
                    reason: "no room".into(),
                },
                folder: None,
            }],
            incoming_files: None,
            layout_saved: true,
            saved_layouts: 3,
            rival_programs: vec!["Input Director".into()],
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["devices"][0]["screensAllowed"], true);
        assert_eq!(json["devices"][0]["watchingScreens"], true);
        assert_eq!(json["rivalPrograms"][0], "Input Director");
        assert_eq!(json["rules"]["shareApps"], true);
        assert_eq!(json["rules"]["mediaControl"], true);
        assert_eq!(json["rules"]["mediaShortcuts"]["playPause"]["key"], "Space");
        assert_eq!(json["layoutSaved"], true);
        assert_eq!(json["savedLayouts"], 3);
        assert_eq!(json["devices"][0]["filesAllowed"], true);
        assert_eq!(json["rules"]["notifyFiles"], true);
        assert_eq!(json["locked"], true);
        assert_eq!(json["enabled"], false);
        assert_eq!(json["desktopFolder"], "C:\\Bureau");
        assert_eq!(json["transfers"][0]["totalBytes"], 10);
        assert_eq!(json["transfers"][0]["stage"]["stage"], "failed");
        assert_eq!(json["transfers"][0]["stage"]["reason"], "no room");
        assert_eq!(json["devices"][0]["canReceiveFiles"], false);
        assert_eq!(json["me"]["model"], "Dell XPS 13");
        assert_eq!(json["heldByFullscreen"], true);
        assert_eq!(json["captureError"], serde_json::Value::Null);
        assert_eq!(json["rules"]["stayInFullscreen"], false);
        assert_eq!(json["rules"]["edgeDelayMs"], 0);
        assert_eq!(json["devices"][0]["status"], "incompatible");
        assert_eq!(json["devices"][0]["needsUpdateForFiles"], true);
        assert_eq!(json["control"]["state"], "home");
    }

    #[test]
    fn doing_things_with_apps_uses_the_format_the_app_sends() {
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"appAction","device":"{DEVICE}","app":"msedge","action":{{"bringUp":{{"monitor":"left"}}}}}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::AppAction {
                device: DEVICE,
                app: "msedge".into(),
                action: AppAction::BringUp {
                    monitor: Some("left".into()),
                },
            }
        );
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"appAction","device":"{DEVICE}","app":"msedge","action":{{"moveTo":{{"from":"left","to":"right"}}}}}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::AppAction {
                device: DEVICE,
                app: "msedge".into(),
                action: AppAction::MoveTo {
                    from: "left".into(),
                    to: "right".into(),
                },
            }
        );
    }

    #[test]
    fn pictures_use_the_format_the_app_sends_and_reads() {
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"screenPicture","id":7,"device":"{DEVICE}","monitor":"m","maxWidth":640,"maxHeight":400,"known":12,"cursor":true}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::ScreenPicture {
                id: 7,
                ask: ScreenAsk {
                    device: DEVICE,
                    monitor: "m".into(),
                    max_width: 640,
                    max_height: 400,
                    known: 12,
                    cursor: true,
                },
            }
        );
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"setScreensAllowed","device":"{DEVICE}","allowed":true}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::SetScreensAllowed {
                device: DEVICE,
                allowed: true,
            }
        );

        let json = serde_json::to_value(PictureAnswer::Picture {
            url: "data:image/jpeg;base64,AA==".into(),
            width: 640,
            height: 360,
            fingerprint: 9,
        })
        .unwrap();
        assert_eq!(json["status"], "picture");
        assert_eq!(json["url"], "data:image/jpeg;base64,AA==");
        assert_eq!(json["fingerprint"], 9);
        for (answer, status) in [
            (PictureAnswer::Unchanged, "unchanged"),
            (PictureAnswer::NotAllowed, "notAllowed"),
            (PictureAnswer::NeedsUpdate, "needsUpdate"),
            (PictureAnswer::Unavailable, "unavailable"),
            (PictureAnswer::Busy, "busy"),
        ] {
            assert_eq!(serde_json::to_value(answer).unwrap()["status"], status);
        }
    }

    /// Both kinds of message share one line of text at a time.
    #[test]
    fn what_comes_from_the_agent_says_what_it_is() {
        let picture = FromAgent::Picture(ScreenPicture {
            id: 3,
            answer: PictureAnswer::Busy,
        });
        let text = serde_json::to_string(&picture).unwrap();
        assert_eq!(text, r#"{"picture":{"id":3,"answer":{"status":"busy"}}}"#);
        assert_eq!(serde_json::from_str::<FromAgent>(&text).unwrap(), picture);
    }

    #[test]
    fn media_uses_the_format_the_app_sends_and_reads() {
        let request: Request = serde_json::from_str(&format!(
            r#"{{"type":"mediaCommand","device":"{DEVICE}","app":"spotify","command":"playPause"}}"#
        ))
        .unwrap();
        assert_eq!(
            request,
            Request::MediaCommand {
                device: DEVICE,
                app: "spotify".into(),
                command: MediaCommand::PlayPause,
            }
        );

        let json = serde_json::to_value(AppView {
            id: "spotify".into(),
            name: "Spotify".into(),
            icon: Some("data:image/png;base64,AA==".into()),
            minimized: true,
            media: Some(MediaState {
                playing: None,
                can_go_back: true,
                can_go_forward: false,
            }),
            has_window: true,
        })
        .unwrap();
        assert_eq!(json["hasWindow"], true);
        assert_eq!(json["icon"], "data:image/png;base64,AA==");
        assert_eq!(json["media"]["playing"], serde_json::Value::Null);
        assert_eq!(json["media"]["canGoBack"], true);
        assert_eq!(json["media"]["canGoForward"], false);
    }

    #[test]
    fn state_uses_the_format_the_app_reads() {
        let view = PairingView {
            peer_name: Some("desk".into()),
            stage: PairingStage::ShowCode {
                code: "123456".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&view).unwrap(),
            r#"{"peerName":"desk","stage":{"stage":"showCode","code":"123456"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ControlView::Controlling { device: DEVICE }).unwrap(),
            format!(r#"{{"state":"controlling","device":"{DEVICE}"}}"#)
        );
    }
}
