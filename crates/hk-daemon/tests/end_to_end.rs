//! Whole agents talking to each other over real encrypted connections, driven
//! through the same local channel the app uses. Only the operating system is
//! simulated, with `FakePlatform`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use hk_core::apps::{MediaCommand, MediaState, RunningApp};
use hk_core::clipboard::{ClipVersion, ClipboardData, ClipboardImage, SharedFile};
use hk_core::device::Fingerprint;
use hk_core::geometry::{Point, Rect};
use hk_core::input::{InputEvent, KeyCode};
use hk_core::layout::{LayoutDoc, Monitor, Placement};
use hk_core::rules::{CrossModifier, NameClash, ReceiveFiles, Rules, Shortcut};
use hk_core::switcher::LocalEvent;
use hk_daemon::{Options, lock_screen, run};
use hk_ipc::{
    ControlView, DeviceStatus, IpcConnection, PairingStage, Request, TransferStage, UiState,
};
use hk_net::{Endpoint, Identity, TrustStore};
use hk_platform::Platform;
use hk_platform::fake::FakePlatform;
use hk_proto::{
    ClipboardBlob, FEATURE_FILES, FEATURE_SEND_FILES, FEATURES, FileAnswer, FileRequest, Message,
    PROTOCOL_VERSION, TransferUpdate,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Long enough for an agent to look again at what's open.
const TICK_AND_A_BIT: Duration = Duration::from_millis(2500);

/// One running agent with a simulated computer, and an app connected to it.
struct Device {
    name: String,
    platform: FakePlatform,
    app: Option<IpcConnection>,
    state: UiState,
    task: JoinHandle<anyhow::Result<()>>,
    dir: PathBuf,
    keep_dir: bool,
    channel: String,
}

fn unique_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn channel_name(id: u32) -> String {
    let process = std::process::id();
    if cfg!(windows) {
        format!(r"\\.\pipe\hackerman-e2e-{process}-{id}")
    } else {
        std::env::temp_dir()
            .join(format!("hackerman-e2e-{process}-{id}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// An empty folder for one agent. Windows reuses process IDs, so one left behind
/// by a crashed run must not be picked up: its old paired devices would be loaded.
fn fresh_dir(id: u32) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hackerman-e2e-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Device {
    async fn start(name: &str) -> Self {
        Self::start_with(name, FakePlatform::with_screens(1)).await
    }

    async fn start_with(name: &str, platform: FakePlatform) -> Self {
        let id = unique_id();
        let dir = fresh_dir(id);
        std::fs::write(dir.join("hackerman.toml"), format!("name = \"{name}\"\n")).unwrap();
        Self::start_in(name, dir, channel_name(id), platform).await
    }

    async fn start_in(name: &str, dir: PathBuf, channel: String, platform: FakePlatform) -> Self {
        let options = Options {
            config_dir: dir,
            port: Some(0),
            channel,
            capture: true,
            discovery: false,
            headless_pairing: false,
        };
        Self::start_as(name, options, platform).await
    }

    async fn start_as(name: &str, options: Options, platform: FakePlatform) -> Self {
        let (dir, channel) = (options.config_dir.clone(), options.channel.clone());
        let task = tokio::spawn(run(options, platform.clone()));

        // On a restart, this can reach the previous agent's channel as it closes,
        // which never answers. Holding on to that would keep the name in use and
        // stop the new agent from opening it, so give up on silence quickly.
        let mut attempts = 0;
        let (app, state) = loop {
            if let Ok(mut app) = hk_ipc::connect(&channel).await
                && let Ok(Ok(Some(state))) =
                    tokio::time::timeout(Duration::from_millis(500), app.recv()).await
            {
                break (app, state);
            }
            attempts += 1;
            assert!(
                attempts < 100 && !task.is_finished(),
                "{name}'s agent never answered. Stopped: {}",
                task.is_finished()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        Self {
            name: name.to_string(),
            platform,
            app: Some(app),
            state,
            task,
            dir,
            keep_dir: false,
            channel,
        }
    }

    fn fingerprint(&self) -> Fingerprint {
        self.state.me.fingerprint
    }

    fn address(&self) -> String {
        format!("127.0.0.1:{}", self.state.me.port)
    }

    fn app(&mut self) -> &mut IpcConnection {
        self.app.as_mut().expect("the app was closed")
    }

    async fn request(&mut self, request: Request) {
        self.app().send(&request).await.unwrap();
    }

    /// Waits until the agent's state satisfies `check`.
    async fn wait_for(&mut self, what: &str, check: impl Fn(&UiState) -> bool) {
        let name = self.name.clone();
        let wait = async {
            while !check(&self.state) {
                self.state = self
                    .app()
                    .recv()
                    .await
                    .unwrap()
                    .expect("the agent stopped unexpectedly");
            }
        };
        if tokio::time::timeout(TIMEOUT, wait).await.is_err() {
            panic!(
                "{name} never reached: {what}\nlast state: {:#?}",
                self.state
            );
        }
    }

    async fn wait_connected_to(&mut self, other: Fingerprint) {
        self.wait_for("connected", |state| {
            let connected = state
                .devices
                .iter()
                .any(|d| d.fingerprint == other && d.status == DeviceStatus::Connected);
            // Its monitors arrive with the connection.
            connected && state.screens.iter().any(|s| s.device == other && s.online)
        })
        .await;
    }

    /// Simulates the mouse moving to this position on this computer.
    fn move_mouse(&self, x: f64, y: f64) -> bool {
        self.platform.move_cursor(Point::new(x, y))
    }

    fn press(&self, code: KeyCode, pressed: bool) -> bool {
        self.platform.local(LocalEvent::Key { code, pressed })
    }

    async fn shut_down(&mut self) {
        self.request(Request::Shutdown).await;
        tokio::time::timeout(TIMEOUT, &mut self.task)
            .await
            .expect("the agent didn't stop")
            .unwrap()
            .unwrap();
    }

    async fn stop(mut self) {
        self.shut_down().await;
    }

    /// Returns what's needed to start this same device again.
    async fn stop_for_restart(mut self) -> (PathBuf, String, FakePlatform) {
        self.shut_down().await;
        self.keep_dir = true;
        (
            self.dir.clone(),
            self.channel.clone(),
            self.platform.clone(),
        )
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.task.abort();
        if !self.keep_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

async fn pair(initiator: &mut Device, responder: &mut Device) {
    let (initiator_id, responder_id) = (initiator.fingerprint(), responder.fingerprint());
    initiator
        .request(Request::PairAddress {
            address: responder.address(),
        })
        .await;
    initiator
        .wait_for("asked for the code", |state| {
            matches!(
                state.pairing.as_ref().map(|p| &p.stage),
                Some(PairingStage::EnterCode)
            )
        })
        .await;
    responder
        .wait_for("showing the code", |state| {
            matches!(
                state.pairing.as_ref().map(|p| &p.stage),
                Some(PairingStage::ShowCode { .. })
            )
        })
        .await;
    let Some(PairingStage::ShowCode { code }) =
        responder.state.pairing.as_ref().map(|p| p.stage.clone())
    else {
        unreachable!();
    };
    initiator.request(Request::SubmitPairingCode { code }).await;

    let paired = |state: &UiState| {
        matches!(
            state.pairing.as_ref().map(|p| &p.stage),
            Some(PairingStage::Paired)
        )
    };
    initiator.wait_for("paired", paired).await;
    responder.wait_for("paired", paired).await;
    initiator.request(Request::DismissPairing).await;
    responder.request(Request::DismissPairing).await;
    initiator.wait_connected_to(responder_id).await;
    responder.wait_connected_to(initiator_id).await;
}

/// Puts `right` against the right edge of `left`, both 1920x1080.
async fn place_side_by_side(editor: &mut Device, left: Fingerprint, right: Fingerprint) {
    editor
        .request(Request::SetPlacements {
            placements: vec![
                Placement {
                    device: left,
                    monitor: "fake-0".into(),
                    x: 0.0,
                    y: 0.0,
                    width: None,
                },
                Placement {
                    device: right,
                    monitor: "fake-0".into(),
                    x: 1920.0,
                    y: 0.0,
                    width: None,
                },
            ],
        })
        .await;
}

fn canvas_x(state: &UiState, device: Fingerprint) -> Option<f64> {
    state
        .screens
        .iter()
        .find(|screen| screen.device == device)
        .map(|screen| screen.x)
}

/// Waits until `device`'s cursor has been sent very close to this position.
async fn wait_for_position_near(device: &Device, x: f64, y: f64) {
    let is_near = |event: &InputEvent| {
        matches!(*event, InputEvent::PointerPosition { x: at_x, y: at_y }
            if (at_x - x).abs() < 0.01 && (at_y - y).abs() < 0.01)
    };
    let wait = async {
        while !device.platform.injected().iter().any(is_near) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{} never got near ({x}, {y}), only {:?}",
                device.name,
                device.platform.injected()
            )
        });
}

/// Waits until `device` has replayed `expected` somewhere in its input.
async fn wait_for_injected(device: &Device, expected: InputEvent) {
    let wait = async {
        while !device.platform.injected().contains(&expected) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{} never got {expected:?}, only {:?}",
                device.name,
                device.platform.injected()
            )
        });
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_pair_with_the_code_and_share_input() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;

    assert!(a.state.devices.iter().any(|d| d.name == "bravo"));
    assert!(b.state.devices.iter().any(|d| d.name == "alpha"));

    // One device's layout edit reaches the other.
    place_side_by_side(&mut a, a_id, b_id).await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;
    b.wait_for("got the layout", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;

    // In the middle of the screen nothing happens.
    assert!(!a.move_mouse(1000.0, 500.0));
    assert!(b.platform.injected().is_empty());

    // Past the edge the mouse crosses over.
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
    assert!(a.platform.cursor_hidden(), "the local cursor should hide");
    a.wait_for("controlling", |s| {
        s.control == ControlView::Controlling { device: b_id }
    })
    .await;
    b.wait_for("controlled", |s| {
        s.control == ControlView::ControlledBy { device: a_id }
    })
    .await;

    // The keyboard follows, and is hidden from the machine it came from.
    assert!(a.press(KeyCode::KeyH, true));
    wait_for_injected(
        &b,
        InputEvent::Key {
            code: KeyCode::KeyH,
            pressed: true,
        },
    )
    .await;

    // Movement is relative to where the local cursor is parked (960, 540 here).
    assert!(a.move_mouse(1010.0, 540.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 50.0, y: 500.0 }).await;

    // Going back over the edge returns home, and the held key is let go there.
    assert!(a.move_mouse(960.0 - 51.0, 540.0));
    wait_for_injected(
        &b,
        InputEvent::Key {
            code: KeyCode::KeyH,
            pressed: false,
        },
    )
    .await;
    a.wait_for("home", |s| s.control == ControlView::Home).await;
    b.wait_for("released", |s| s.control == ControlView::Home)
        .await;
    assert_eq!(a.platform.cursor(), Point::new(1918.0, 500.0));
    assert!(
        !a.platform.cursor_hidden(),
        "the local cursor should be back"
    );
    assert!(!a.move_mouse(1000.0, 500.0));
}

/// Input one way and layout edits the other, so both directions of the connection
/// are busy at once. Abandoning a half-read message here would corrupt the stream.
#[tokio::test(flavor = "multi_thread")]
async fn heavy_traffic_both_ways_keeps_the_connection_healthy() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;
    place_side_by_side(&mut a, a_id, b_id).await;
    b.wait_for("got the layout", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;
    assert!(a.move_mouse(1919.0, 500.0));

    for round in 0..200 {
        // Sweeps the remote cursor right and back, staying off the edge it came in by.
        let offset = if round % 2 == 0 { 1.0 } else { -1.0 };
        for _ in 0..20 {
            a.move_mouse(960.0 + offset, 540.0);
        }
        // Each edit is a much larger message, so it can span several packets.
        let y = f64::from(round);
        b.request(Request::SetPlacements {
            placements: vec![Placement {
                device: b_id,
                monitor: "fake-0".into(),
                x: 1920.0,
                y,
                width: None,
            }],
        })
        .await;
        tokio::task::yield_now().await;
    }

    // A last move and a last edit both still arrive, in a session that stayed up.
    let final_key = InputEvent::Key {
        code: KeyCode::KeyZ,
        pressed: true,
    };
    assert!(a.press(KeyCode::KeyZ, true));
    wait_for_injected(&b, final_key).await;
    a.wait_for("the last edit", |s| {
        s.screens
            .iter()
            .any(|screen| screen.device == b_id && screen.y == 199.0)
    })
    .await;
    assert!(
        a.state
            .devices
            .iter()
            .any(|d| d.fingerprint == b_id && d.status == DeviceStatus::Connected)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_code_does_not_pair() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    a.request(Request::PairAddress {
        address: b.address(),
    })
    .await;
    a.wait_for("asked for the code", |s| {
        matches!(
            s.pairing.as_ref().map(|p| &p.stage),
            Some(PairingStage::EnterCode)
        )
    })
    .await;
    b.wait_for("showing the code", |s| {
        matches!(
            s.pairing.as_ref().map(|p| &p.stage),
            Some(PairingStage::ShowCode { .. })
        )
    })
    .await;
    let Some(PairingStage::ShowCode { code }) = b.state.pairing.as_ref().map(|p| p.stage.clone())
    else {
        unreachable!();
    };
    let wrong = if code == "000000" { "000001" } else { "000000" };
    a.request(Request::SubmitPairingCode { code: wrong.into() })
        .await;

    let failed = |s: &UiState| {
        matches!(
            s.pairing.as_ref().map(|p| &p.stage),
            Some(PairingStage::Failed { .. })
        )
    };
    a.wait_for("failed", failed).await;
    b.wait_for("failed", failed).await;
    assert!(a.state.devices.is_empty());
    assert!(b.state.devices.is_empty());
    // Only each device's own monitor is on its canvas.
    assert_eq!(a.state.screens.len(), 1);
    assert_eq!(b.state.screens.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_with_no_app_open_refuses_to_pair() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    // Closing the app leaves the agent running with no screen to show a code on.
    b.app = None;
    tokio::time::sleep(Duration::from_millis(300)).await;

    a.request(Request::PairAddress {
        address: b.address(),
    })
    .await;
    a.wait_for("refused", |s| {
        matches!(
            s.pairing.as_ref().map(|p| &p.stage),
            Some(PairingStage::Failed { .. })
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mouse_hops_through_three_devices() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let mut c = Device::start("charlie").await;
    let (a_id, b_id, c_id) = (a.fingerprint(), b.fingerprint(), c.fingerprint());
    pair(&mut a, &mut b).await;
    pair(&mut a, &mut c).await;

    a.request(Request::SetPlacements {
        placements: vec![
            Placement {
                device: a_id,
                monitor: "fake-0".into(),
                x: 0.0,
                y: 0.0,
                width: None,
            },
            Placement {
                device: b_id,
                monitor: "fake-0".into(),
                x: 1920.0,
                y: 0.0,
                width: None,
            },
            Placement {
                device: c_id,
                monitor: "fake-0".into(),
                x: 3840.0,
                y: 0.0,
                width: None,
            },
        ],
    })
    .await;
    // All three, because automatic placement may have put any one of them there.
    a.wait_for("placed", |s| {
        canvas_x(s, a_id) == Some(0.0)
            && canvas_x(s, b_id) == Some(1920.0)
            && canvas_x(s, c_id) == Some(3840.0)
    })
    .await;

    // Over to the first device...
    assert!(a.move_mouse(1919.0, 300.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 300.0 }).await;
    // ...across it to its far edge (parked at 960, 540)...
    assert!(a.move_mouse(960.0 + 1919.0, 540.0));
    wait_for_injected(
        &b,
        InputEvent::PointerPosition {
            x: 1919.0,
            y: 300.0,
        },
    )
    .await;
    // ...and on to the third, without coming home in between.
    assert!(a.move_mouse(961.0, 540.0));
    wait_for_injected(&c, InputEvent::PointerPosition { x: 0.0, y: 300.0 }).await;
    a.wait_for("controlling the third", |s| {
        s.control == ControlView::Controlling { device: c_id }
    })
    .await;
    b.wait_for("let go", |s| s.control == ControlView::Home)
        .await;

    // Keys now go to the third device only.
    a.press(KeyCode::KeyC, true);
    let key = InputEvent::Key {
        code: KeyCode::KeyC,
        pressed: true,
    };
    wait_for_injected(&c, key).await;
    assert!(!b.platform.injected().contains(&key));
}

#[tokio::test(flavor = "multi_thread")]
async fn losing_the_connection_brings_control_home() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;
    place_side_by_side(&mut a, a_id, b_id).await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;

    assert!(a.move_mouse(1919.0, 500.0));
    a.wait_for("controlling", |s| {
        s.control == ControlView::Controlling { device: b_id }
    })
    .await;

    // The other computer goes away while it's in control.
    b.stop().await;
    a.wait_for("home again", |s| s.control == ControlView::Home)
        .await;
    a.wait_for("shown as offline", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == b_id && d.status == DeviceStatus::Offline)
    })
    .await;

    // Input stays here, the cursor is back where it left, and the device keeps
    // its place on the canvas.
    assert_eq!(a.platform.cursor(), Point::new(1918.0, 500.0));
    assert!(!a.press(KeyCode::KeyA, true));
    assert!(!a.move_mouse(1919.0, 500.0));
    assert_eq!(canvas_x(&a.state, b_id), Some(1920.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_held_on_a_device_are_released_if_the_controller_vanishes() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;
    place_side_by_side(&mut a, a_id, b_id).await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;
    b.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;

    a.move_mouse(1919.0, 500.0);
    a.press(KeyCode::ShiftLeft, true);
    let pressed = InputEvent::Key {
        code: KeyCode::ShiftLeft,
        pressed: true,
    };
    wait_for_injected(&b, pressed).await;

    a.stop().await;
    wait_for_injected(
        &b,
        InputEvent::Key {
            code: KeyCode::ShiftLeft,
            pressed: false,
        },
    )
    .await;
    b.wait_for("not controlled any more", |s| {
        s.control == ControlView::Home
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_and_layout_survive_a_restart() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;
    place_side_by_side(&mut a, a_id, b_id).await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;

    let (dir, channel, platform) = a.stop_for_restart().await;
    b.wait_for("offline", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Offline)
    })
    .await;

    let mut a = Device::start_in("alpha", dir, channel, platform).await;
    assert_eq!(a.fingerprint(), a_id, "the identity should be kept");
    assert_eq!(canvas_x(&a.state, b_id), Some(1920.0));
    // A restarted device gets a new port in this test, and has to find the other
    // at its old address: it's the one that knows where to go.
    a.wait_connected_to(b_id).await;
    b.wait_connected_to(a_id).await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugging_in_a_monitor_reaches_the_other_device() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let b_id = b.fingerprint();
    pair(&mut a, &mut b).await;
    let screens_before = a.state.screens.iter().filter(|s| s.device == b_id).count();
    assert_eq!(screens_before, 1);

    let mut monitors = b.platform.monitors();
    monitors.push(Monitor {
        id: "new".into(),
        name: "New monitor".into(),
        bounds: Rect::new(1920.0, 0.0, 2560.0, 1440.0),
        scale: 1.0,
        primary: false,
    });
    b.platform.set_monitors(monitors);

    a.wait_for("the new monitor", |s| {
        s.screens
            .iter()
            .any(|screen| screen.device == b_id && screen.monitor == "new")
    })
    .await;
    // It appears next to the monitor it sits beside in the other computer's settings.
    let first = a
        .state
        .screens
        .iter()
        .find(|s| s.device == b_id && s.monitor == "fake-0")
        .unwrap();
    let new = a
        .state
        .screens
        .iter()
        .find(|s| s.device == b_id && s.monitor == "new")
        .unwrap();
    assert_eq!(new.x, first.x + first.width);
    assert_eq!(new.y, first.y);
}

#[tokio::test(flavor = "multi_thread")]
async fn forgetting_a_device_disconnects_it() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;

    a.request(Request::Forget { device: b_id }).await;
    a.wait_for("forgotten", |s| s.devices.is_empty()).await;
    assert!(a.state.screens.iter().all(|s| s.device == a_id));
    b.wait_for("disconnected", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Offline)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn renaming_reaches_the_other_device() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    pair(&mut a, &mut b).await;
    a.request(Request::Rename {
        name: "workstation".into(),
    })
    .await;
    a.wait_for("renamed", |s| s.me.name == "workstation").await;
    b.wait_for("sees the new name", |s| {
        s.devices.iter().any(|d| d.name == "workstation")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_agent_can_run_on_a_channel() {
    let first = Device::start("alpha").await;
    let dir = fresh_dir(unique_id());
    let options = Options {
        config_dir: dir.clone(),
        port: Some(0),
        channel: first.channel.clone(),
        capture: false,
        discovery: false,
        headless_pairing: false,
    };
    let result = run(options, FakePlatform::with_screens(1)).await;
    let _ = std::fs::remove_dir_all(dir);
    assert!(result.unwrap_err().to_string().contains("already running"));
}

fn text(content: &str) -> ClipboardData {
    ClipboardData {
        text: Some(content.into()),
        ..Default::default()
    }
}

/// Waits until `device`'s clipboard holds exactly `expected`.
async fn wait_for_clipboard(device: &Device, expected: &ClipboardData) {
    let wait = async {
        while device.platform.clipboard() != *expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{}'s clipboard never became {expected:?}, it's {:?}",
                device.name,
                device.platform.clipboard()
            )
        });
}

/// Two paired devices side by side, with the second to the right of the first.
async fn side_by_side() -> (Device, Device) {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;
    place_side_by_side(&mut a, a_id, b_id).await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;
    b.wait_for("placed", |s| {
        canvas_x(s, a_id) == Some(0.0) && canvas_x(s, b_id) == Some(1920.0)
    })
    .await;
    (a, b)
}

/// Long enough for anything sent over the local connection to have arrived.
async fn let_things_settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_text_follows_the_cursor_but_goes_nowhere_before_that() {
    let (a, b) = side_by_side().await;
    a.move_mouse(100.0, 100.0);
    a.platform.copy(text("from alpha"));

    // Not copied to a computer the user isn't on.
    let_things_settle().await;
    assert!(b.platform.clipboard().is_empty());

    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_clipboard(&b, &text("from alpha")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn copying_on_a_controlled_device_comes_home_with_the_cursor() {
    let (a, b) = side_by_side().await;
    a.move_mouse(100.0, 100.0);
    a.platform.copy(text("old"));
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_clipboard(&b, &text("old")).await;

    b.platform.copy(text("new, from bravo"));
    let_things_settle().await;
    assert_eq!(a.platform.clipboard(), text("old"), "still away from home");

    // Back over the edge (the cursor is parked at 960, 540 on this computer).
    assert!(a.move_mouse(959.0, 540.0));
    wait_for_clipboard(&a, &text("new, from bravo")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn formatted_text_and_large_images_arrive_intact() {
    let (a, b) = side_by_side().await;
    a.move_mouse(100.0, 100.0);
    let formatted = ClipboardData {
        text: Some("bold".into()),
        html: Some("<b>bold</b>".into()),
        image: None,
    };
    a.platform.copy(formatted.clone());
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_clipboard(&b, &formatted).await;
    assert!(a.move_mouse(959.0, 540.0));

    // Much larger than any ordinary message, and input keeps working meanwhile.
    let image = ClipboardData {
        image: Some(ClipboardImage {
            width: 4000,
            height: 3000,
            png: (0..5_000_000u32).map(|i| (i % 253) as u8).collect(),
        }),
        ..Default::default()
    };
    a.move_mouse(100.0, 100.0);
    a.platform.copy(image.clone());
    assert!(a.move_mouse(1919.0, 500.0));
    assert!(a.press(KeyCode::KeyQ, true));
    wait_for_injected(
        &b,
        InputEvent::Key {
            code: KeyCode::KeyQ,
            pressed: true,
        },
    )
    .await;
    wait_for_clipboard(&b, &image).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_computer_nobody_is_using_cannot_replace_the_clipboard() {
    let (a, b) = side_by_side().await;
    a.move_mouse(100.0, 100.0);
    a.platform.copy(text("what the user copied"));
    let_things_settle().await;

    // Some background program changes the clipboard on the idle computer.
    b.platform.copy(text("from a background program"));
    let_things_settle().await;
    a.move_mouse(200.0, 100.0);
    let_things_settle().await;
    assert_eq!(a.platform.clipboard(), text("what the user copied"));
}

#[tokio::test(flavor = "multi_thread")]
async fn using_a_computer_directly_brings_the_clipboard_to_it() {
    let (a, b) = side_by_side().await;
    a.move_mouse(100.0, 100.0);
    a.platform.copy(text("from alpha"));
    let_things_settle().await;
    assert!(b.platform.clipboard().is_empty());

    // Someone picks up the other computer's own mouse.
    b.move_mouse(500.0, 500.0);
    wait_for_clipboard(&b, &text("from alpha")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_clipboard_can_be_turned_off_on_either_side() {
    let (mut a, b) = side_by_side().await;
    a.request(Request::SetRules {
        rules: Rules {
            clipboard: false,
            ..Rules::default()
        },
    })
    .await;
    a.wait_for("the rule", |s| !s.rules.clipboard).await;

    a.move_mouse(100.0, 100.0);
    a.platform.copy(text("private"));
    assert!(a.move_mouse(1919.0, 500.0));
    let_things_settle().await;
    assert!(b.platform.clipboard().is_empty());

    // Nor does anything come in.
    b.platform.copy(text("from bravo"));
    assert!(a.move_mouse(959.0, 540.0));
    let_things_settle().await;
    assert_eq!(a.platform.clipboard(), text("private"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_connecting_later_still_gets_the_clipboard() {
    let (mut a, b) = side_by_side().await;
    a.move_mouse(100.0, 100.0);
    a.platform.copy(text("copied while alone"));
    let_things_settle().await;

    let mut c = Device::start("charlie").await;
    let c_id = c.fingerprint();
    pair(&mut a, &mut c).await;
    assert!(b.platform.clipboard().is_empty());

    // A straight jump. Paired devices are numbered from 2, in fingerprint order.
    a.press(KeyCode::ControlLeft, true);
    a.press(KeyCode::AltLeft, true);
    let number = if a.state.devices[0].fingerprint == c_id {
        KeyCode::Digit2
    } else {
        KeyCode::Digit3
    };
    assert!(a.press(number, true));
    wait_for_clipboard(&c, &text("copied while alone")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_lock_works_from_the_app_and_the_keyboard() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();

    a.request(Request::SetLocked { locked: true }).await;
    a.wait_for("locked", |s| s.locked).await;
    assert!(!a.move_mouse(1919.0, 500.0), "locked: stays here");

    assert!(!a.press(KeyCode::ControlLeft, true));
    assert!(!a.press(KeyCode::AltLeft, true));
    assert!(a.press(KeyCode::KeyL, true), "the shortcut key is hidden");
    assert!(a.press(KeyCode::KeyL, false));
    a.wait_for("unlocked", |s| !s.locked).await;

    assert!(a.move_mouse(1919.0, 500.0));
    a.wait_for("controlling", |s| {
        s.control == ControlView::Controlling { device: b_id }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shortcut_jumps_to_the_middle_of_another_device() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    a.move_mouse(300.0, 200.0);
    a.press(KeyCode::ControlLeft, true);
    a.press(KeyCode::AltLeft, true);

    assert!(a.press(KeyCode::Digit2, true));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 960.0, y: 540.0 }).await;
    a.wait_for("controlling", |s| {
        s.control == ControlView::Controlling { device: b_id }
    })
    .await;
    a.press(KeyCode::Digit2, false);

    // And number 1 comes back to where the cursor left from.
    assert!(a.press(KeyCode::Digit1, true));
    a.wait_for("home", |s| s.control == ControlView::Home).await;
    assert_eq!(a.platform.cursor(), Point::new(300.0, 200.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fullscreen_app_can_keep_the_cursor_here() {
    let (mut a, _b) = side_by_side().await;
    a.platform.set_fullscreen(true);

    a.request(Request::SetRules {
        rules: Rules {
            stay_in_fullscreen: true,
            ..Rules::default()
        },
    })
    .await;
    a.wait_for("held", |s| s.held_by_fullscreen).await;
    assert!(!a.move_mouse(1919.0, 500.0));

    a.platform.set_fullscreen(false);
    a.wait_for("released", |s| !s.held_by_fullscreen).await;
    assert!(a.move_mouse(1919.0, 500.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_survive_a_restart() {
    let mut a = Device::start("alpha").await;
    let rules = Rules {
        clipboard: false,
        shortcuts: false,
        stay_in_fullscreen: true,
        edge_delay_ms: 250,
        ..Rules::default()
    };
    a.request(Request::SetRules {
        rules: rules.clone(),
    })
    .await;
    a.wait_for("the rules", |s| s.rules.edge_delay_ms == 250)
        .await;
    let (dir, channel, platform) = a.stop_for_restart().await;

    let a = Device::start_in("alpha", dir, channel, platform).await;
    assert_eq!(a.state.rules, rules);
}

/// A paired device running another version should be reported as such, rather
/// than just looking like it's offline.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_on_another_version_is_reported() {
    let other_version = Identity::generate().unwrap();
    let other_id = other_version.fingerprint();

    let id = unique_id();
    let dir = fresh_dir(id);
    std::fs::write(
        dir.join("devices.json"),
        format!(r#"[{{"fingerprint":"{other_id}","name":"other-pc"}}]"#),
    )
    .unwrap();
    let mut a = Device::start_in(
        "alpha",
        dir,
        channel_name(id),
        FakePlatform::with_screens(1),
    )
    .await;

    let endpoint = Endpoint::bind(
        &other_version,
        "127.0.0.1:0".parse().unwrap(),
        TrustStore::new([a.fingerprint()]),
    )
    .unwrap();
    let address = a.address().parse().unwrap();
    let link = endpoint.connect(address, a.fingerprint()).await.unwrap();
    let (mut sender, mut receiver) = link.split();
    sender
        .send(&Message::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            device_name: "other-pc".into(),
            monitors: vec![],
            layout: LayoutDoc::default(),
            model: None,
            enabled: true,
            features: 0,
        })
        .await
        .unwrap();

    a.wait_for("reported", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == other_id && d.status == DeviceStatus::Incompatible)
    })
    .await;
    // It gets this device's introduction, so it can report the mismatch as well,
    // and then the connection closes.
    assert!(matches!(receiver.recv().await, Ok(Message::Hello { .. })));
    assert!(receiver.recv().await.is_err());
}

fn monitor(id: &str, x: f64, y: f64, width: f64, height: f64, scale: f64) -> Monitor {
    Monitor {
        id: id.into(),
        name: id.into(),
        bounds: Rect::new(x, y, width, height),
        scale,
        primary: x == 0.0 && y == 0.0,
    }
}

fn placed(device: Fingerprint, monitor: &str, x: f64, y: f64) -> Placement {
    Placement {
        device,
        monitor: monitor.into(),
        x,
        y,
        width: None,
    }
}

/// The real desk: a computer with one screen at each end, and two other computers
/// standing between them. In its own display settings its two screens touch.
#[tokio::test(flavor = "multi_thread")]
async fn the_cursor_travels_across_a_desk_with_a_computer_split_around_two_others() {
    let mut amelkis = Device::start_with(
        "amelkis",
        FakePlatform::new(vec![
            monitor("built-in", 0.0, 0.0, 2560.0, 1600.0, 1.5),
            monitor("maestro", 2560.0, 76.0, 1920.0, 1080.0, 1.0),
        ]),
    )
    .await;
    let mut thinkpad = Device::start("thinkpad").await;
    let mut main = Device::start("main").await;
    let (a_id, t_id, m_id) = (
        amelkis.fingerprint(),
        thinkpad.fingerprint(),
        main.fingerprint(),
    );
    pair(&mut amelkis, &mut thinkpad).await;
    pair(&mut amelkis, &mut main).await;
    pair(&mut thinkpad, &mut main).await;

    // Built-in, ThinkPad, MAIN, Maestro, from left to right. The built-in screen is
    // about 1707 wide on the canvas at 150%.
    let built_in_width = 2560.0 / 1.5;
    thinkpad
        .request(Request::SetPlacements {
            placements: vec![
                placed(a_id, "built-in", 0.0, 0.0),
                placed(t_id, "fake-0", built_in_width, 0.0),
                placed(m_id, "fake-0", built_in_width + 1920.0, 0.0),
                placed(a_id, "maestro", built_in_width + 3840.0, 0.0),
            ],
        })
        .await;
    let arranged = |s: &UiState| {
        s.screens
            .iter()
            .any(|screen| screen.monitor == "maestro" && screen.x == built_in_width + 3840.0)
            && canvas_x(s, m_id) == Some(built_in_width + 1920.0)
    };
    amelkis.wait_for("the layout", arranged).await;
    thinkpad.wait_for("the layout", arranged).await;
    main.wait_for("the layout", arranged).await;

    // Starting with Amelkis's own mouse, on its built-in screen. Its display
    // settings would take the cursor straight onto the Maestro from here.
    amelkis.platform.warp_for_test(Point::new(2559.0, 800.0));
    assert!(amelkis.move_mouse(2561.0, 800.0), "hidden from Windows");
    assert!(amelkis.platform.cursor_hidden());
    // 800 of 1600 pixels down, at 150%, is 533 logical pixels down the canvas.
    wait_for_position_near(&thinkpad, 0.0, 533.33).await;
    amelkis
        .wait_for("controlling the ThinkPad", |s| {
            s.control == ControlView::Controlling { device: t_id }
        })
        .await;

    // Across the ThinkPad to MAIN (the parked cursor is at 960, 540 here).
    assert!(amelkis.move_mouse(960.0 + 1919.0, 540.0));
    assert!(amelkis.move_mouse(961.0, 540.0));
    wait_for_position_near(&main, 0.0, 533.33).await;
    thinkpad
        .wait_for("let go", |s| s.control == ControlView::Home)
        .await;

    // Across MAIN and home onto Amelkis's far screen, its own Maestro.
    assert!(amelkis.move_mouse(960.0 + 1919.0, 540.0));
    assert!(amelkis.move_mouse(961.0, 540.0));
    amelkis
        .wait_for("home", |s| s.control == ControlView::Home)
        .await;
    assert!(!amelkis.platform.cursor_hidden());
    let cursor = amelkis.platform.cursor();
    assert_eq!(cursor.x, 2561.0, "just inside the Maestro's left edge");
    assert!(
        cursor.y > 76.0 && cursor.y < 76.0 + 1080.0,
        "on the Maestro"
    );

    // Going left from the Maestro goes back to MAIN, not to the built-in screen.
    amelkis.platform.warp_for_test(Point::new(2560.0, 500.0));
    assert!(amelkis.move_mouse(2559.0, 500.0));
    amelkis
        .wait_for("controlling MAIN", |s| {
            s.control == ControlView::Controlling { device: m_id }
        })
        .await;
}

/// If a computer in the middle goes away, the far screen must still be reachable.
#[tokio::test(flavor = "multi_thread")]
async fn a_separated_screen_stays_reachable_when_what_was_between_goes_offline() {
    let mut a = Device::start_with(
        "alpha",
        FakePlatform::new(vec![
            monitor("first", 0.0, 0.0, 1920.0, 1080.0, 1.0),
            monitor("last", 1920.0, 0.0, 1920.0, 1080.0, 1.0),
        ]),
    )
    .await;
    let mut b = Device::start("bravo").await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    pair(&mut a, &mut b).await;
    a.request(Request::SetPlacements {
        placements: vec![
            placed(a_id, "first", 0.0, 0.0),
            placed(b_id, "fake-0", 1920.0, 0.0),
            placed(a_id, "last", 3840.0, 0.0),
        ],
    })
    .await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;

    b.stop().await;
    a.wait_for("offline", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == b_id && d.status == DeviceStatus::Offline)
    })
    .await;

    // Windows' own arrangement takes over.
    a.platform.warp_for_test(Point::new(1919.0, 500.0));
    assert!(!a.move_mouse(1921.0, 500.0));
    assert_eq!(a.platform.cursor(), Point::new(1921.0, 500.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn resizing_a_screen_changes_where_the_cursor_lands_and_reaches_the_other_device() {
    let (mut a, mut b) = side_by_side().await;
    let b_id = b.fingerprint();

    // Bravo drawn half size against alpha's right edge.
    a.request(Request::SetPlacements {
        placements: vec![Placement {
            width: Some(960.0),
            ..placed(b_id, "fake-0", 1920.0, 0.0)
        }],
    })
    .await;
    let resized = |s: &UiState| {
        s.screens.iter().any(|screen| {
            screen.device == b_id
                && screen.width == 960.0
                && screen.height == 540.0
                && screen.custom_width == Some(960.0)
        })
    };
    a.wait_for("resized", resized).await;
    b.wait_for("resized there too", resized).await;

    // 270 of alpha's rows down is halfway down the half-size screen.
    assert!(a.move_mouse(1919.0, 270.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 540.0 }).await;
    assert!(a.move_mouse(959.0, 540.0), "back over the edge");
    a.wait_for("home", |s| s.control == ControlView::Home).await;
    assert_eq!(a.platform.cursor(), Point::new(1918.0, 270.0));

    // Below the bottom of the half-size screen, nothing lines up.
    assert!(!a.move_mouse(1919.0, 700.0));

    // Moving it keeps its size, and clearing the size goes back to the usual one.
    a.request(Request::SetPlacements {
        placements: vec![Placement {
            width: None,
            ..placed(b_id, "fake-0", 1920.0, 0.0)
        }],
    })
    .await;
    let usual = |s: &UiState| {
        s.screens
            .iter()
            .any(|screen| screen.device == b_id && screen.width == 1920.0)
    };
    a.wait_for("the usual size", usual).await;
    b.wait_for("the usual size there too", usual).await;
}

/// Screens don't have to touch: the cursor crosses a gap of any size, keeps its
/// height, and comes back the same way.
#[tokio::test(flavor = "multi_thread")]
async fn the_cursor_crosses_a_gap_between_screens_that_do_not_touch() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.request(Request::SetPlacements {
        placements: vec![
            placed(a_id, "fake-0", 0.0, 0.0),
            placed(b_id, "fake-0", 9000.0, 100.0),
        ],
    })
    .await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(9000.0))
        .await;

    // 100 lower on the canvas, so row 500 here lines up with row 400 there.
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 400.0 }).await;

    assert!(a.move_mouse(959.0, 540.0));
    a.wait_for("home", |s| s.control == ControlView::Home).await;
    assert_eq!(a.platform.cursor(), Point::new(1918.0, 500.0));

    // Above where the other screen starts, nothing is in line with the cursor.
    assert!(!a.move_mouse(1919.0, 50.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn each_device_shows_the_make_and_model_of_the_others() {
    let thinkpad = FakePlatform::with_screens(1);
    thinkpad.set_model("Lenovo ThinkPad X1 Carbon Gen 12");
    let dell = FakePlatform::with_screens(1);
    dell.set_model("Dell Latitude 7420");
    let mut a = Device::start_with("alpha", thinkpad).await;
    let mut b = Device::start_with("bravo", dell).await;
    assert_eq!(
        a.state.me.model.as_deref(),
        Some("Lenovo ThinkPad X1 Carbon Gen 12")
    );
    pair(&mut a, &mut b).await;
    assert_eq!(
        a.state.devices[0].model.as_deref(),
        Some("Dell Latitude 7420")
    );
    assert_eq!(
        b.state.devices[0].model.as_deref(),
        Some("Lenovo ThinkPad X1 Carbon Gen 12")
    );
}

fn status_of(state: &UiState, device: Fingerprint) -> Option<DeviceStatus> {
    state
        .devices
        .iter()
        .find(|d| d.fingerprint == device)
        .map(|d| d.status)
}

impl Device {
    async fn turn(&mut self, enabled: bool) {
        self.request(Request::SetEnabled { enabled }).await;
        self.wait_for("turned on or off", |s| s.enabled == enabled)
            .await;
    }

    async fn wait_until_status(&mut self, device: Fingerprint, status: DeviceStatus) {
        self.wait_for("the other device's status", |s| {
            status_of(s, device) == Some(status)
        })
        .await;
    }
}

fn key(code: KeyCode, pressed: bool) -> InputEvent {
    InputEvent::Key { code, pressed }
}

/// Turning off a device someone is controlling sends the cursor home and lets go of
/// whatever they held down there. Turning it on again works at once.
#[tokio::test(flavor = "multi_thread")]
async fn turning_a_device_off_sends_the_cursor_home_and_turning_it_on_works_at_once() {
    let (mut a, mut b) = side_by_side().await;
    let b_id = b.fingerprint();
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
    assert!(a.press(KeyCode::KeyQ, true));
    wait_for_injected(&b, key(KeyCode::KeyQ, true)).await;

    b.turn(false).await;
    a.wait_until_status(b_id, DeviceStatus::Off).await;
    a.wait_for("home", |s| s.control == ControlView::Home).await;
    assert_eq!(a.platform.cursor(), Point::new(1918.0, 500.0));
    assert!(!a.platform.cursor_hidden());
    wait_for_injected(&b, key(KeyCode::KeyQ, false)).await;
    b.wait_for("not controlled", |s| s.control == ControlView::Home)
        .await;

    // Nothing reaches it now, and the edge is just an edge.
    let before = b.platform.injected().len();
    assert!(!a.move_mouse(1919.0, 500.0));
    let_things_settle().await;
    assert_eq!(b.platform.injected().len(), before);

    b.turn(true).await;
    a.wait_until_status(b_id, DeviceStatus::Connected).await;
    assert!(a.move_mouse(1919.0, 600.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 600.0 }).await;
}

/// The device that's off keeps its own keyboard and mouse: no hooks, no shortcuts.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_turned_off_keeps_its_own_input() {
    let (mut a, mut b) = side_by_side().await;
    let a_id = a.fingerprint();
    a.turn(false).await;
    b.wait_until_status(a_id, DeviceStatus::Off).await;

    assert!(!a.move_mouse(1919.0, 500.0), "the cursor doesn't cross");
    assert!(!a.press(KeyCode::ControlLeft, true));
    assert!(!a.press(KeyCode::AltLeft, true));
    assert!(!a.press(KeyCode::Digit2, true), "shortcuts don't work");
    let_things_settle().await;
    assert!(b.platform.injected().is_empty());
    assert_eq!(a.state.control, ControlView::Home);
    for code in [KeyCode::Digit2, KeyCode::AltLeft, KeyCode::ControlLeft] {
        assert!(!a.press(code, false));
    }

    // Keys held when it was turned off don't count as held afterwards.
    a.turn(true).await;
    b.wait_until_status(a_id, DeviceStatus::Connected).await;
    assert!(!a.press(KeyCode::Digit2, true), "no modifiers are held");
    assert!(!a.press(KeyCode::Digit2, false));
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
}

/// It's skipped like an offline device, so what's beyond it stays reachable.
#[tokio::test(flavor = "multi_thread")]
async fn the_cursor_passes_over_a_device_that_is_turned_off() {
    let mut a = Device::start("alpha").await;
    let mut b = Device::start("bravo").await;
    let mut c = Device::start("charlie").await;
    let (a_id, b_id, c_id) = (a.fingerprint(), b.fingerprint(), c.fingerprint());
    pair(&mut a, &mut b).await;
    pair(&mut a, &mut c).await;
    a.request(Request::SetPlacements {
        placements: vec![
            placed(a_id, "fake-0", 0.0, 0.0),
            placed(b_id, "fake-0", 1920.0, 0.0),
            placed(c_id, "fake-0", 3840.0, 0.0),
        ],
    })
    .await;
    a.wait_for("placed", |s| {
        canvas_x(s, b_id) == Some(1920.0) && canvas_x(s, c_id) == Some(3840.0)
    })
    .await;

    b.turn(false).await;
    a.wait_until_status(b_id, DeviceStatus::Off).await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&c, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
    assert!(b.platform.injected().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn turned_off_stays_off_after_a_restart_and_the_others_are_told() {
    let (mut a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.turn(false).await;
    b.wait_until_status(a_id, DeviceStatus::Off).await;

    let (dir, channel, platform) = a.stop_for_restart().await;
    b.wait_until_status(a_id, DeviceStatus::Offline).await;

    let mut a = Device::start_in("alpha", dir, channel, platform).await;
    assert!(!a.state.enabled);
    a.wait_connected_to(b_id).await;
    b.wait_until_status(a_id, DeviceStatus::Off).await;
    assert!(!a.move_mouse(1919.0, 500.0), "nothing is captured");

    a.turn(true).await;
    b.wait_until_status(a_id, DeviceStatus::Connected).await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_clipboard_stays_out_of_it_while_turned_off() {
    let (mut a, mut b) = side_by_side().await;
    let a_id = a.fingerprint();
    a.turn(false).await;
    b.wait_until_status(a_id, DeviceStatus::Off).await;

    // What's copied there stays there.
    a.platform.copy(text("private"));
    b.move_mouse(100.0, 100.0);
    let_things_settle().await;
    assert!(b.platform.clipboard().is_empty());

    // Nothing comes in.
    b.platform.copy(text("from bravo"));
    let_things_settle().await;
    assert_eq!(a.platform.clipboard(), text("private"));

    // Turning it on doesn't replace what's there, nor send it anywhere.
    a.turn(true).await;
    b.wait_until_status(a_id, DeviceStatus::Connected).await;
    a.move_mouse(200.0, 100.0);
    b.move_mouse(200.0, 100.0);
    let_things_settle().await;
    assert_eq!(a.platform.clipboard(), text("private"));
    assert_eq!(b.platform.clipboard(), text("from bravo"));

    // What's copied from now on travels again.
    b.platform.copy(text("newer, from bravo"));
    let_things_settle().await;
    a.move_mouse(300.0, 100.0);
    wait_for_clipboard(&a, &text("newer, from bravo")).await;
}

/// Copying on a device that's off mustn't be replaced later by something it had
/// already been offered, and hadn't fetched.
#[tokio::test(flavor = "multi_thread")]
async fn what_was_copied_while_off_beats_an_older_offer() {
    let (mut a, b) = side_by_side().await;
    b.move_mouse(100.0, 100.0);
    b.platform.copy(text("offered before turning off"));
    let_things_settle().await;
    assert!(a.platform.clipboard().is_empty(), "not fetched yet");

    a.turn(false).await;
    a.platform.copy(text("copied while off"));
    let_things_settle().await;
    a.turn(true).await;

    a.move_mouse(200.0, 100.0);
    let_things_settle().await;
    assert_eq!(a.platform.clipboard(), text("copied while off"));
}

/// A folder for one test's files. Whatever a run leaves behind, such as after a failure
/// to look into, goes when the next run starts.
fn temp_folder(name: &str) -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    let root = ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join("hackerman-e2e-files");
        let _ = std::fs::remove_dir_all(&root);
        root
    });
    let folder = root.join(unique_id().to_string()).join(name);
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

async fn wait_for_files(device: &Device, count: usize) -> Vec<SharedFile> {
    let wait = async {
        while device.platform.files_to_paste().len() != count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{} never got {count} files, only {:?}",
                device.name,
                device.platform.files_to_paste()
            )
        });
    device.platform.files_to_paste()
}

/// Reads a file as pasting it would, on a thread the async runtime knows nothing about,
/// since that's what Windows' calls arrive on.
async fn read_pasted(device: &Device, index: usize) -> std::io::Result<Vec<u8>> {
    let platform = device.platform.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut reader = platform
            .read_pasted_file(index, 0)
            .expect("no files to paste");
        let mut content = Vec::new();
        let result = reader.read_to_end(&mut content).map(|_| content);
        let _ = done_tx.send(result);
    });
    done_rx.await.unwrap()
}

/// Different at every position, so that anything out of place shows.
fn big_content(size: usize) -> Vec<u8> {
    (0..size)
        .map(|at| ((at / 251 + at % 241) % 256) as u8)
        .collect()
}

/// This is what looked like a broken clipboard: copying a file sent nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn copied_files_follow_the_cursor_and_arrive_when_pasted() {
    let (a, b) = side_by_side().await;
    let folder = temp_folder("from alpha");
    std::fs::create_dir_all(folder.join("album/sous dossier")).unwrap();
    std::fs::write(folder.join("album/sous dossier/été.txt"), "deep").unwrap();
    std::fs::write(folder.join("album/a.txt"), "first version").unwrap();
    std::fs::write(folder.join("alone.txt"), "alone").unwrap();

    a.move_mouse(100.0, 100.0);
    a.platform
        .copy_files(vec![folder.join("album"), folder.join("alone.txt")]);
    let_things_settle().await;
    assert!(b.platform.files_to_paste().is_empty(), "not there yet");

    assert!(a.move_mouse(1919.0, 500.0));
    let files = wait_for_files(&b, 5).await;
    let names: Vec<_> = files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(
        names,
        [
            "album",
            "album/a.txt",
            "album/sous dossier",
            "album/sous dossier/été.txt",
            "alone.txt"
        ]
    );
    assert!(files[0].is_dir && !files[1].is_dir);
    assert_eq!(files[3].size, "deep".len() as u64);

    // Nothing has moved yet: the content is read now, as it is now.
    std::fs::write(folder.join("album/a.txt"), "other version").unwrap();
    assert_eq!(read_pasted(&b, 1).await.unwrap(), b"other version");
    assert_eq!(read_pasted(&b, 3).await.unwrap(), b"deep");
    assert_eq!(read_pasted(&b, 4).await.unwrap(), b"alone");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_also_come_home_with_the_cursor() {
    let (a, mut b) = side_by_side().await;
    let folder = temp_folder("from bravo");
    std::fs::write(folder.join("report.txt"), "from the other PC").unwrap();

    assert!(a.move_mouse(1919.0, 500.0));
    b.wait_for("being controlled", |s| {
        matches!(s.control, ControlView::ControlledBy { .. })
    })
    .await;
    b.platform.copy_files(vec![folder.join("report.txt")]);
    let_things_settle().await;
    assert!(a.move_mouse(959.0, 540.0));
    wait_for_files(&a, 1).await;
    assert_eq!(read_pasted(&a, 0).await.unwrap(), b"from the other PC");
}

/// A big file on the same connection mustn't get in the way of the mouse.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_file_arrives_intact_while_the_mouse_keeps_working() {
    const SIZE: usize = 200 * 1024 * 1024;
    let (mut a, b) = side_by_side().await;
    let folder = temp_folder("large");
    let content = big_content(SIZE);
    std::fs::write(folder.join("big.bin"), &content).unwrap();

    a.move_mouse(100.0, 100.0);
    a.platform.copy_files(vec![folder.join("big.bin")]);
    let_things_settle().await;
    assert!(a.move_mouse(1919.0, 500.0));
    assert_eq!(wait_for_files(&b, 1).await[0].size, SIZE as u64);

    let platform = b.platform.clone();
    let reading = tokio::task::spawn_blocking(move || {
        let mut reader = platform.read_pasted_file(0, 0).unwrap();
        let mut received = Vec::with_capacity(SIZE);
        reader.read_to_end(&mut received).unwrap();
        received
    });

    let mut slowest = Duration::ZERO;
    let mut moves = 0;
    while !reading.is_finished() {
        // The cursor is parked in the middle here, so this moves one pixel right there.
        let started = Instant::now();
        assert!(a.move_mouse(961.0, 541.0));
        let before = b.platform.injected().len();
        let arrived = async {
            while b.platform.injected().len() == before {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(200), arrived).await;
        slowest = slowest.max(started.elapsed());
        moves += 1;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    println!("while {SIZE} bytes went by, {moves} mouse moves took at most {slowest:?}");
    assert!(moves > 0);
    assert!(slowest < Duration::from_millis(250), "{slowest:?}");

    assert!(
        reading.await.unwrap() == content,
        "the file changed on the way"
    );
    a.wait_for("still connected", |s| {
        matches!(s.control, ControlView::Controlling { .. })
    })
    .await;
    let _ = std::fs::remove_dir_all(folder);
}

#[tokio::test(flavor = "multi_thread")]
async fn copying_something_else_does_not_break_a_paste_in_progress() {
    const SIZE: usize = 30 * 1024 * 1024;
    let (a, b) = side_by_side().await;
    let folder = temp_folder("in progress");
    let content = big_content(SIZE);
    std::fs::write(folder.join("big.bin"), &content).unwrap();

    a.move_mouse(100.0, 100.0);
    a.platform.copy_files(vec![folder.join("big.bin")]);
    let_things_settle().await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_files(&b, 1).await;

    let platform = b.platform.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let reading = tokio::task::spawn_blocking(move || {
        let mut reader = platform.read_pasted_file(0, 0).unwrap();
        let mut received = vec![0; SIZE];
        reader.read_exact(&mut received[..1024]).unwrap();
        let _ = started_tx.send(());
        reader.read_exact(&mut received[1024..]).unwrap();
        received
    });
    started_rx.await.unwrap();
    a.platform.copy(text("something else"));

    assert!(reading.await.unwrap() == content);
    let _ = std::fs::remove_dir_all(folder);
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_stops_files_going_out_even_partway() {
    const SIZE: usize = 100 * 1024 * 1024;
    let (mut a, b) = side_by_side().await;
    let folder = temp_folder("stopped");
    std::fs::write(folder.join("big.bin"), big_content(SIZE)).unwrap();

    a.move_mouse(100.0, 100.0);
    a.platform.copy_files(vec![folder.join("big.bin")]);
    let_things_settle().await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_files(&b, 1).await;

    let platform = b.platform.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let reading = tokio::task::spawn_blocking(move || {
        let mut reader = platform.read_pasted_file(0, 0).unwrap();
        let mut piece = vec![0; 1024];
        reader.read_exact(&mut piece).unwrap();
        let _ = started_tx.send(());
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).map(|_| rest.len())
    });
    started_rx.await.unwrap();
    a.turn(false).await;

    let result = reading.await.unwrap();
    assert!(result.is_err(), "kept going: {result:?}");
    let _ = std::fs::remove_dir_all(folder);
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_stops_files_coming_in_too() {
    let (a, mut b) = side_by_side().await;
    let folder = temp_folder("disabled here");
    std::fs::write(folder.join("file.txt"), "content").unwrap();
    a.move_mouse(100.0, 100.0);
    a.platform.copy_files(vec![folder.join("file.txt")]);
    let_things_settle().await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_files(&b, 1).await;

    b.turn(false).await;
    let error = read_pasted(&b, 0).await.unwrap_err();
    assert!(error.to_string().contains("disabled"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_has_gone_is_an_error_not_an_empty_file() {
    let (a, b) = side_by_side().await;
    let folder = temp_folder("gone");
    std::fs::write(folder.join("file.txt"), "content").unwrap();
    a.move_mouse(100.0, 100.0);
    a.platform.copy_files(vec![folder.join("file.txt")]);
    let_things_settle().await;
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_files(&b, 1).await;

    std::fs::remove_file(folder.join("file.txt")).unwrap();
    assert!(read_pasted(&b, 0).await.is_err());
}

/// Each device only knows of the copies made on devices it's paired with, so the two at
/// the ends can't count their copies together. This used to lose about half the time.
#[tokio::test(flavor = "multi_thread")]
async fn copies_from_devices_not_paired_with_each_other_all_count() {
    let mut middle = Device::start("middle").await;
    let mut one = Device::start("one").await;
    let mut other = Device::start("other").await;
    pair(&mut middle, &mut one).await;
    pair(&mut middle, &mut other).await;
    // The one that loses a tie goes second, which is the case that went wrong.
    let (first, second) = if one.fingerprint() > other.fingerprint() {
        (&one, &other)
    } else {
        (&other, &one)
    };

    first.move_mouse(100.0, 100.0);
    first.platform.copy(text("copied first"));
    let_things_settle().await;
    middle.move_mouse(100.0, 100.0);
    wait_for_clipboard(&middle, &text("copied first")).await;

    second.move_mouse(100.0, 100.0);
    second.platform.copy(text("copied second"));
    let_things_settle().await;
    middle.move_mouse(200.0, 100.0);
    wait_for_clipboard(&middle, &text("copied second")).await;
}

/// A paired device whose every message the test writes by hand, to act like another
/// version of Hackerman, or one that's misbehaving.
struct HandWrittenPeer {
    id: Fingerprint,
    _endpoint: Endpoint,
    sender: hk_net::LinkSender,
    receiver: hk_net::LinkReceiver,
    blobs: hk_net::BlobChannel,
}

impl HandWrittenPeer {
    async fn next_offer(&mut self) -> ClipVersion {
        loop {
            let message = tokio::time::timeout(TIMEOUT, self.receiver.recv()).await;
            if let Message::ClipboardOffer(version) = message.unwrap().unwrap() {
                return version;
            }
        }
    }

    /// The clipboard this peer asks for, if it arrives.
    async fn ask_for(&mut self, version: ClipVersion) -> Option<ClipboardBlob> {
        self.sender
            .send(&Message::ClipboardRequest(version))
            .await
            .unwrap();
        let bytes = tokio::time::timeout(Duration::from_millis(500), self.blobs.receive()).await;
        Some(hk_proto::decode_clipboard(&bytes.ok()?.unwrap()).unwrap())
    }

    async fn request_file(&self, request: FileRequest) -> (FileAnswer, Vec<u8>) {
        let mut reader = self
            .blobs
            .request(&hk_proto::encode_file_request(&request).unwrap())
            .await
            .unwrap();
        let answer = reader.read_message().await.unwrap();
        let answer = hk_proto::decode_file_answer(&answer).unwrap();
        let mut content = Vec::new();
        let mut piece = [0; 4096];
        loop {
            match reader.read(&mut piece).await {
                Ok(0) | Err(_) => return (answer, content),
                Ok(count) => content.extend_from_slice(&piece[..count]),
            }
        }
    }
}

async fn device_with_hand_written_peer(features: u32) -> (Device, HandWrittenPeer) {
    let identity = Identity::generate().unwrap();
    let peer_id = identity.fingerprint();
    let id = unique_id();
    let dir = fresh_dir(id);
    std::fs::write(
        dir.join("devices.json"),
        format!(r#"[{{"fingerprint":"{peer_id}","name":"hand-written"}}]"#),
    )
    .unwrap();
    let mut device = Device::start_in(
        "alpha",
        dir,
        channel_name(id),
        FakePlatform::with_screens(1),
    )
    .await;

    let endpoint = Endpoint::bind(
        &identity,
        "127.0.0.1:0".parse().unwrap(),
        TrustStore::new([device.fingerprint()]),
    )
    .unwrap();
    let address = device.address().parse().unwrap();
    let link = endpoint
        .connect(address, device.fingerprint())
        .await
        .unwrap();
    let blobs = link.blobs();
    let (mut sender, receiver) = link.split();
    sender
        .send(&Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "hand-written".into(),
            monitors: vec![monitor("old", 0.0, 0.0, 1920.0, 1080.0, 1.0)],
            layout: LayoutDoc::default(),
            model: None,
            enabled: true,
            features,
        })
        .await
        .unwrap();
    device
        .wait_until_status(peer_id, DeviceStatus::Connected)
        .await;
    let peer = HandWrittenPeer {
        id: peer_id,
        _endpoint: endpoint,
        sender,
        receiver,
        blobs,
    };
    (device, peer)
}

/// Mixed versions keep working together: a PC that hasn't got file sharing yet just
/// doesn't get files, and everything else carries on.
#[tokio::test(flavor = "multi_thread")]
async fn a_pc_on_an_older_version_stays_connected_and_keeps_its_clipboard() {
    let (mut a, mut old) = device_with_hand_written_peer(0).await;
    let old_id = old.id;
    a.wait_for("shown as needing an update", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == old_id && d.needs_update_for_files)
    })
    .await;

    let folder = temp_folder("older peer");
    std::fs::write(folder.join("file.txt"), "content").unwrap();
    a.move_mouse(100.0, 100.0);
    a.platform.copy_files(vec![folder.join("file.txt")]);
    let version = old.next_offer().await;
    assert!(
        old.ask_for(version).await.is_none(),
        "it couldn't read that"
    );

    // Text still goes to it, and it's still connected.
    a.platform.copy(text("still works"));
    let version = old.next_offer().await;
    let blob = old.ask_for(version).await.expect("the clipboard");
    assert_eq!(blob.data, text("still works"));
    assert!(blob.files.is_empty());
    assert_eq!(status_of(&a.state, old_id), Some(DeviceStatus::Connected));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_with_file_sharing_is_not_told_to_update() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    a.wait_until_status(b_id, DeviceStatus::Connected).await;
    let device = a.state.devices.iter().find(|d| d.fingerprint == b_id);
    assert!(!device.unwrap().needs_update_for_files);
}

/// Being paired doesn't mean being able to read just anything on this computer.
#[tokio::test(flavor = "multi_thread")]
async fn only_files_that_were_copied_can_be_read() {
    let (a, mut peer) = device_with_hand_written_peer(FEATURES).await;
    let folder = temp_folder("offered");
    std::fs::create_dir_all(folder.join("sub")).unwrap();
    std::fs::write(folder.join("copied.txt"), "you may read this").unwrap();
    std::fs::write(folder.join("private.txt"), "but not this").unwrap();

    a.move_mouse(100.0, 100.0);
    a.platform
        .copy_files(vec![folder.join("copied.txt"), folder.join("sub")]);
    let version = peer.next_offer().await;
    let blob = peer.ask_for(version).await.expect("the list");
    assert_eq!(blob.files.len(), 2);

    let ask = |index, offset, version| FileRequest {
        version,
        index,
        offset,
    };
    let (answer, content) = peer.request_file(ask(0, 0, version)).await;
    assert_eq!(answer, FileAnswer::Content);
    assert_eq!(content, b"you may read this");

    // Partway in, for picking up where a dropped connection left off.
    let (_, content) = peer.request_file(ask(0, 8, version)).await;
    assert_eq!(content, b"read this");

    // Past the end of the list.
    let (answer, content) = peer.request_file(ask(2, 0, version)).await;
    assert!(matches!(answer, FileAnswer::Refused(_)), "{answer:?}");
    assert!(content.is_empty());

    // A version that was never offered.
    let never = ClipVersion {
        counter: version.counter + 1,
        owner: version.owner,
    };
    let (answer, _) = peer.request_file(ask(0, 0, never)).await;
    assert!(matches!(answer, FileAnswer::Refused(_)), "{answer:?}");

    // A folder has no content to give.
    let (answer, _) = peer.request_file(ask(1, 0, version)).await;
    assert!(matches!(answer, FileAnswer::Refused(_)), "{answer:?}");
}

/// Names from another computer are about to reach File Explorer.
#[tokio::test(flavor = "multi_thread")]
async fn file_names_that_could_escape_the_folder_are_refused() {
    let (a, mut peer) = device_with_hand_written_peer(FEATURES).await;
    let version = ClipVersion {
        counter: u64::MAX / 2,
        owner: peer.id,
    };
    peer.sender
        .send(&Message::ClipboardOffer(version))
        .await
        .unwrap();
    let_things_settle().await;

    let list = |path: &str| ClipboardBlob {
        version,
        data: ClipboardData::default(),
        files: vec![SharedFile {
            path: path.into(),
            size: 4,
            is_dir: false,
            modified_ms: None,
        }],
        save_to_folder: false,
    };
    let send = |blob: ClipboardBlob| {
        let blobs = peer.blobs.clone();
        async move {
            let bytes = hk_proto::encode_clipboard(&blob).unwrap();
            blobs.send(&bytes).await.unwrap();
        }
    };
    a.move_mouse(100.0, 100.0);
    send(list("../../Startup/evil.exe")).await;
    let_things_settle().await;
    assert!(a.platform.files_to_paste().is_empty());

    send(list("fine.txt")).await;
    wait_for_files(&a, 1).await;
}

/// The list said how much there is, which is all that a paste will ever take.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_cannot_send_more_than_it_said() {
    let (a, mut peer) = device_with_hand_written_peer(FEATURES).await;
    let version = ClipVersion {
        counter: u64::MAX / 2,
        owner: peer.id,
    };
    peer.sender
        .send(&Message::ClipboardOffer(version))
        .await
        .unwrap();
    let_things_settle().await;
    a.move_mouse(100.0, 100.0);
    let blob = ClipboardBlob {
        version,
        data: ClipboardData::default(),
        files: vec![SharedFile {
            path: "small.txt".into(),
            size: 5,
            is_dir: false,
            modified_ms: None,
        }],
        save_to_folder: false,
    };
    peer.blobs
        .send(&hk_proto::encode_clipboard(&blob).unwrap())
        .await
        .unwrap();
    wait_for_files(&a, 1).await;

    let blobs = peer.blobs.clone();
    let server = tokio::spawn(async move {
        let (_, mut writer) = blobs.next_request().await.unwrap().read().await.unwrap();
        let answer = hk_proto::encode_file_answer(&FileAnswer::Content).unwrap();
        writer.write_message(&answer).await.unwrap();
        // It stops being read long before this ends.
        let _ = writer.write(&vec![b'x'; 10 * 1024 * 1024]).await;
    });
    assert_eq!(read_pasted(&a, 0).await.unwrap(), b"xxxxx");
    server.abort();
}

/// Two paired devices, each with a Desktop folder of its own.
async fn two_with_desktops() -> (Device, Device, PathBuf, PathBuf) {
    let (a_desktop, b_desktop) = (temp_folder("a desktop"), temp_folder("b desktop"));
    let a_platform = FakePlatform::with_screens(1);
    a_platform.set_desktop_folder(a_desktop.clone());
    let b_platform = FakePlatform::with_screens(1);
    b_platform.set_desktop_folder(b_desktop.clone());
    let mut a = Device::start_with("alpha", a_platform).await;
    let mut b = Device::start_with("bravo", b_platform).await;
    pair(&mut a, &mut b).await;
    for device in [&mut a, &mut b] {
        device
            .set_rule(|rules| rules.receive_files = ReceiveFiles::AnyDevice)
            .await;
    }
    (a, b, a_desktop, b_desktop)
}

impl Device {
    async fn drop_files(&mut self, device: Fingerprint, paths: &[&Path]) {
        let before = self.state.transfers.len();
        let paths = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        self.request(Request::SendFiles { device, paths }).await;
        self.wait_for("the transfer to show", |s| s.transfers.len() > before)
            .await;
    }

    async fn set_rule(&mut self, change: impl FnOnce(&mut Rules)) {
        let mut rules = self.state.rules.clone();
        change(&mut rules);
        let wanted = rules.clone();
        self.request(Request::SetRules { rules }).await;
        self.wait_for("the new rule", |s| s.rules == wanted).await;
    }

    /// Waits for the newest transfer to reach an end, and returns how it ended.
    async fn wait_for_transfer_end(&mut self) -> hk_ipc::TransferView {
        self.wait_for("a transfer to end", |s| {
            s.transfers.last().is_some_and(|t| t.stage.is_over())
        })
        .await;
        self.state.transfers.last().unwrap().clone()
    }
}

fn parts_left(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(folder).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(parts_left(&path));
        } else if path.to_string_lossy().ends_with(".hackerman-part") {
            found.push(path);
        }
    }
    found
}

/// Drop a file on a screen, and it's on that PC's Desktop.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_dropped_on_another_pc_lands_on_its_desktop() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = temp_folder("source").join("rapport été.txt");
    std::fs::write(&source, "the content").unwrap();

    a.drop_files(b_id, &[&source]).await;

    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert!(sent.sending);
    assert_eq!(sent.label, "rapport été.txt");
    assert_eq!(sent.total_bytes, 11);
    assert_eq!(sent.done_bytes, 11);
    assert_eq!(
        sent.folder.as_deref(),
        Some(&*b_desktop.display().to_string())
    );

    let saved = b_desktop.join("rapport été.txt");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "the content");
    let received = b.wait_for_transfer_end().await;
    assert_eq!(received.stage, TransferStage::Done);
    assert!(!received.sending);
    assert_eq!(received.peer, a.fingerprint());
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_arrive_whole_in_the_folder_chosen_in_settings() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let chosen = temp_folder("chosen").join("Hackerman reçus");
    let chosen_text = chosen.display().to_string();
    b.set_rule(|rules| rules.drop_folder = Some(chosen_text))
        .await;

    let source = temp_folder("source");
    std::fs::create_dir_all(source.join("album/sous dossier")).unwrap();
    std::fs::write(source.join("album/sous dossier/deep.txt"), "deep").unwrap();
    std::fs::write(source.join("other.txt"), "other").unwrap();
    a.drop_files(b_id, &[&source.join("album"), &source.join("other.txt")])
        .await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert_eq!(sent.label, "album and 1 more");

    assert_eq!(
        std::fs::read_to_string(chosen.join("album/sous dossier/deep.txt")).unwrap(),
        "deep"
    );
    assert_eq!(
        std::fs::read_to_string(chosen.join("other.txt")).unwrap(),
        "other"
    );
    let on_desktop: Vec<_> = std::fs::read_dir(&b_desktop).unwrap().flatten().collect();
    assert!(on_desktop.is_empty(), "{on_desktop:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_dropped_on_this_pc_is_copied_to_its_own_desktop() {
    let (mut a, _b, a_desktop, _) = two_with_desktops().await;
    let me = a.fingerprint();
    let source = temp_folder("source").join("mine.txt");
    std::fs::write(&source, "mine").unwrap();
    a.drop_files(me, &[&source]).await;
    let done = a.wait_for_transfer_end().await;
    assert_eq!(done.stage, TransferStage::Done, "{done:?}");
    assert_eq!(
        std::fs::read_to_string(a_desktop.join("mine.txt")).unwrap(),
        "mine"
    );
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        "mine",
        "still there"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_name_clash_setting_decides_what_happens_to_a_file_already_there() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = temp_folder("source").join("report.txt");
    std::fs::write(&source, "new").unwrap();
    std::fs::write(b_desktop.join("report.txt"), "old").unwrap();
    let read = |name: &str| std::fs::read_to_string(b_desktop.join(name)).unwrap();

    a.drop_files(b_id, &[&source]).await;
    a.wait_for_transfer_end().await;
    assert_eq!(read("report.txt"), "old");
    assert_eq!(
        read("report (1).txt"),
        "new",
        "keeps both, unless told otherwise"
    );

    b.set_rule(|rules| rules.name_clash = NameClash::Skip).await;
    std::fs::write(&source, "newer").unwrap();
    a.request(Request::DismissTransfer {
        id: a.state.transfers.last().unwrap().id.clone(),
    })
    .await;
    a.wait_for("dismissed", |s| s.transfers.is_empty()).await;
    a.drop_files(b_id, &[&source]).await;
    let skipped = a.wait_for_transfer_end().await;
    assert_eq!(skipped.stage, TransferStage::Done, "{skipped:?}");
    assert_eq!(read("report.txt"), "old");

    b.set_rule(|rules| rules.name_clash = NameClash::Replace)
        .await;
    a.request(Request::DismissTransfer {
        id: a.state.transfers.last().unwrap().id.clone(),
    })
    .await;
    a.wait_for("dismissed", |s| s.transfers.is_empty()).await;
    a.drop_files(b_id, &[&source]).await;
    a.wait_for_transfer_end().await;
    assert_eq!(read("report.txt"), "newer");
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_first_waits_for_an_answer() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    b.set_rule(|rules| rules.receive_files = ReceiveFiles::Ask)
        .await;
    let source = temp_folder("source").join("question.txt");
    std::fs::write(&source, "12345").unwrap();

    a.drop_files(b_id, &[&source]).await;
    b.wait_for("being asked", |s| s.incoming_files.is_some())
        .await;
    let asked = b.state.incoming_files.clone().unwrap();
    assert_eq!(asked.peer_name, "alpha");
    assert_eq!(asked.label, "question.txt");
    assert_eq!(asked.total_bytes, 5);
    assert!(
        !b_desktop.join("question.txt").exists(),
        "not until it's accepted"
    );

    b.answer_files(true, false).await;
    b.wait_for("no longer asking", |s| s.incoming_files.is_none())
        .await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert_eq!(
        std::fs::read_to_string(b_desktop.join("question.txt")).unwrap(),
        "12345"
    );
}

impl Device {
    /// Waits to be asked about some files, and answers.
    async fn answer_files(&mut self, accept: bool, always: bool) {
        self.wait_for("being asked", |s| s.incoming_files.is_some())
            .await;
        let id = self.state.incoming_files.clone().unwrap().id;
        self.request(Request::AnswerIncomingFiles { id, accept, always })
            .await;
    }

    async fn wait_for_files_allowed(&mut self, device: Fingerprint, allowed: bool) {
        self.wait_for("the PC to be allowed or not", |s| {
            s.devices
                .iter()
                .any(|d| d.fingerprint == device && d.files_allowed == allowed)
        })
        .await;
    }
}

impl Device {
    /// Waits for this agent to stop by itself, as it does when the helper takes over.
    async fn wait_until_it_steps_aside(mut self) -> (PathBuf, String, FakePlatform) {
        tokio::time::timeout(TIMEOUT, &mut self.task)
            .await
            .expect("the agent didn't step aside")
            .unwrap()
            .unwrap();
        self.keep_dir = true;
        (
            self.dir.clone(),
            self.channel.clone(),
            self.platform.clone(),
        )
    }

    /// Starts the lock-screen helper from a copy of a user's settings, on simulated screens
    /// that are the same as the user's, since it's the same computer.
    async fn start_lock_screen_helper(
        name: &str,
        user_settings: &Path,
        approved: &[Fingerprint],
    ) -> Self {
        let id = unique_id();
        let dir = fresh_dir(id);
        lock_screen::copy_identity(user_settings, &dir).unwrap();
        lock_screen::copy_settings(user_settings, &dir, approved).unwrap();
        assert!(lock_screen::has_settings(&dir));
        let mut options = lock_screen::options(dir, channel_name(id));
        // Tests don't use real ports or look around the network.
        options.port = Some(0);
        options.discovery = false;
        Self::start_as(name, options, FakePlatform::with_screens(1)).await
    }
}

/// Lock a PC, and a paired PC's keyboard and mouse can still reach it.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_pc_is_handed_to_the_helper_and_comes_back_when_unlocked() {
    let (a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());

    // The screen locks on the PC on the left. Its agent stops, and the other PC notices.
    a.platform.set_should_step_aside(true);
    let (dir, channel, platform) = a.wait_until_it_steps_aside().await;
    b.wait_for("it to go offline", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Offline)
    })
    .await;

    // The helper is the same computer to everyone, with the same place on the canvas.
    let mut helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    assert_eq!(helper.fingerprint(), a_id, "it has to be the same identity");
    helper.wait_connected_to(b_id).await;
    b.wait_connected_to(a_id).await;
    assert_eq!(canvas_x(&b.state, a_id), Some(0.0));
    assert_eq!(canvas_x(&b.state, b_id), Some(1920.0));

    // The other PC's mouse crosses onto the lock screen, and its keyboard types there.
    assert!(b.move_mouse(0.0, 500.0));
    wait_for_injected(
        &helper,
        InputEvent::PointerPosition {
            x: 1919.0,
            y: 500.0,
        },
    )
    .await;
    assert!(b.press(KeyCode::KeyA, true));
    wait_for_injected(
        &helper,
        InputEvent::Key {
            code: KeyCode::KeyA,
            pressed: true,
        },
    )
    .await;
    assert!(b.press(KeyCode::KeyA, false));

    // Unlocked: the helper goes, the agent returns, and everything is as it was.
    helper.stop().await;
    platform.set_should_step_aside(false);
    let mut a = Device::start_in("alpha", dir, channel, platform).await;
    assert_eq!(a.fingerprint(), a_id);
    a.wait_connected_to(b_id).await;
    b.wait_connected_to(a_id).await;
    a.wait_for("the same layout", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;
}

/// Someone at a locked PC mustn't be able to reach the user's other PCs with its keyboard, and
/// typing a password on it has to keep working whatever other PCs are doing.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_pcs_own_keyboard_and_mouse_stay_on_it() {
    let (a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.platform.set_should_step_aside(true);
    let (dir, ..) = a.wait_until_it_steps_aside().await;
    let mut helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    helper.wait_connected_to(b_id).await;
    b.wait_connected_to(a_id).await;

    // Pushed against the edge next to the other PC, it stays here, and nothing is hidden.
    assert!(!helper.move_mouse(1919.0, 500.0));
    assert!(!helper.press(KeyCode::KeyA, true));
    let_things_settle().await;
    assert!(
        b.platform.injected().is_empty(),
        "nothing reached the other PC"
    );
    helper
        .wait_for("still at home", |s| s.control == ControlView::Home)
        .await;
    assert!(helper.state.capture_error.is_none());
    let _ = std::fs::remove_dir_all(dir);
}

/// Nothing leaves a locked PC or lands on it, whatever the user has chosen for when it's in use.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_pc_shares_no_clipboard_and_takes_no_files() {
    let (mut a, mut b, _, _) = two_with_desktops().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    place_side_by_side(&mut a, a_id, b_id).await;
    a.wait_for("placed", |s| canvas_x(s, b_id) == Some(1920.0))
        .await;
    assert_eq!(a.state.rules.receive_files, ReceiveFiles::AnyDevice);
    assert!(a.state.rules.clipboard);

    a.platform.set_should_step_aside(true);
    let (dir, ..) = a.wait_until_it_steps_aside().await;
    let mut helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    helper.wait_connected_to(b_id).await;
    b.wait_connected_to(a_id).await;
    assert!(!helper.state.rules.clipboard);
    assert_eq!(helper.state.rules.receive_files, ReceiveFiles::Never);

    // Copying on the other PC and crossing over brings nothing with it.
    b.platform.copy(text("a secret"));
    let_things_settle().await;
    b.wait_for("placed", |s| canvas_x(s, a_id) == Some(0.0))
        .await;
    assert!(b.move_mouse(0.0, 500.0));
    wait_for_injected(
        &helper,
        InputEvent::PointerPosition {
            x: 1919.0,
            y: 500.0,
        },
    )
    .await;
    let_things_settle().await;
    assert_eq!(
        helper.platform.read_clipboard().unwrap(),
        ClipboardData::default(),
        "nothing arrived"
    );

    // And files sent to it are turned away.
    let file = a_file_called("while-locked.txt");
    b.drop_files(a_id, &[&file]).await;
    let sent = b.wait_for_transfer_end().await;
    let TransferStage::Failed { reason } = &sent.stage else {
        panic!("{sent:?}");
    };
    assert!(reason.contains("doesn't accept files"), "{reason}");
    assert!(helper.state.incoming_files.is_none());
    assert!(helper.state.transfers.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// The Disable button means off at the lock screen as well.
#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_pc_is_still_off_at_the_lock_screen() {
    let (mut a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.request(Request::SetEnabled { enabled: false }).await;
    a.wait_for("disabled", |s| !s.enabled).await;

    a.platform.set_should_step_aside(true);
    let (dir, ..) = a.wait_until_it_steps_aside().await;
    let mut helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    assert!(!helper.state.enabled);
    helper
        .wait_for("connected, but off", |s| {
            s.devices
                .iter()
                .any(|d| d.fingerprint == b_id && d.status == DeviceStatus::Connected)
        })
        .await;
    b.wait_for("it's shown as off", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Off)
    })
    .await;
    assert!(!b.move_mouse(0.0, 500.0), "the cursor skips it");
    let_things_settle().await;
    assert!(helper.platform.injected().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// Forgetting a PC has to count at the lock screen, or it could still type there.
#[tokio::test(flavor = "multi_thread")]
async fn a_pc_forgotten_before_the_lock_cant_reach_the_lock_screen() {
    let (mut a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.request(Request::Forget { device: b_id }).await;
    a.wait_for("forgotten", |s| s.devices.is_empty()).await;
    b.wait_for("cut off", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Offline)
    })
    .await;

    a.platform.set_should_step_aside(true);
    let (dir, ..) = a.wait_until_it_steps_aside().await;
    let helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    assert!(helper.state.devices.is_empty());
    let_things_settle().await;
    assert!(helper.platform.injected().is_empty());
    assert!(
        b.state
            .devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Offline),
        "it doesn't let the forgotten PC connect"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Being paired isn't enough: any of the user's programs could add a device to that list, so
/// reaching the sign-in screen takes an administrator's approval as well.
#[tokio::test(flavor = "multi_thread")]
async fn a_pc_that_has_not_been_approved_cant_reach_the_lock_screen() {
    let (a, mut b) = side_by_side().await;
    let a_id = a.fingerprint();
    a.platform.set_should_step_aside(true);
    let (dir, ..) = a.wait_until_it_steps_aside().await;
    let is_offline = |s: &UiState| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.status == DeviceStatus::Offline)
    };
    b.wait_for("it to go offline", is_offline).await;

    // It's in the user's own list, as a PC paired after this was set up would be.
    let helper = Device::start_lock_screen_helper("alpha", &dir, &[]).await;
    assert!(helper.state.devices.is_empty());
    assert_eq!(helper.fingerprint(), a_id);
    let_things_settle().await;

    assert!(is_offline(&b.state));
    assert!(!b.move_mouse(0.0, 500.0));
    assert!(helper.platform.injected().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// Nobody can see a pairing code at a locked PC. No app can connect to the real helper, whose
/// channel only the system can open, and with no app open an agent refuses to pair. Otherwise,
/// someone on the network could keep guessing the code, and get the lock screen's keyboard.
#[tokio::test(flavor = "multi_thread")]
async fn nobody_can_pair_with_a_locked_pc() {
    let (a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    a.platform.set_should_step_aside(true);
    let (dir, ..) = a.wait_until_it_steps_aside().await;
    let mut helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    helper.app = None; // As it really is.
    let_things_settle().await;

    let mut stranger = Device::start("stranger").await;
    stranger
        .request(Request::PairAddress {
            address: helper.address(),
        })
        .await;
    stranger
        .wait_for("being refused", |s| {
            matches!(
                s.pairing.as_ref().map(|p| &p.stage),
                Some(PairingStage::Failed { .. })
            )
        })
        .await;
    assert!(
        stranger
            .state
            .devices
            .iter()
            .all(|d| d.status != DeviceStatus::Connected)
    );
    drop(b);
    let _ = std::fs::remove_dir_all(dir);
}

/// Two paired devices that haven't changed how they receive files.
async fn two_new_to_each_other() -> (Device, Device, PathBuf) {
    let b_desktop = temp_folder("b desktop");
    let b_platform = FakePlatform::with_screens(1);
    b_platform.set_desktop_folder(b_desktop.clone());
    let mut a = Device::start("alpha").await;
    let mut b = Device::start_with("bravo", b_platform).await;
    pair(&mut a, &mut b).await;
    (a, b, b_desktop)
}

fn a_file_called(name: &str) -> PathBuf {
    let source = temp_folder("source").join(name);
    std::fs::write(&source, name).unwrap();
    source
}

/// A PC that has never sent files here has to be let in.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_files_from_a_pc_are_asked_about_and_then_it_can_be_allowed() {
    let (mut a, mut b, b_desktop) = two_new_to_each_other().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    assert_eq!(b.state.rules.receive_files, ReceiveFiles::AskFirst);
    let first = a_file_called("first.txt");

    a.drop_files(b_id, &[&first]).await;
    b.wait_for("being asked", |s| s.incoming_files.is_some())
        .await;
    assert!(
        !b_desktop.join("first.txt").exists(),
        "not until it's allowed"
    );

    b.answer_files(true, true).await;
    b.wait_for_files_allowed(a_id, true).await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert!(b_desktop.join("first.txt").exists());

    // From now on, no question.
    let second = a_file_called("second.txt");
    a.drop_files(b_id, &[&second]).await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert!(b_desktop.join("second.txt").exists());
    assert!(b.state.incoming_files.is_none());

    // That's remembered.
    let (dir, channel, platform) = b.stop_for_restart().await;
    let mut b = Device::start_in("bravo", dir, channel, platform).await;
    assert!(
        b.state
            .devices
            .iter()
            .any(|d| d.fingerprint == a_id && d.files_allowed)
    );

    // And it can be taken back.
    b.request(Request::SetFilesAllowed {
        device: a_id,
        allowed: false,
    })
    .await;
    b.wait_for_files_allowed(a_id, false).await;
    a.wait_connected_to(b_id).await;
    b.wait_connected_to(a_id).await;
    let third = a_file_called("third.txt");
    a.drop_files(b_id, &[&third]).await;
    b.wait_for("being asked again", |s| s.incoming_files.is_some())
        .await;
    assert!(!b_desktop.join("third.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn saying_yes_just_this_once_asks_again_next_time() {
    let (mut a, mut b, b_desktop) = two_new_to_each_other().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let once = a_file_called("once.txt");
    a.drop_files(b_id, &[&once]).await;
    b.answer_files(true, false).await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert!(b_desktop.join("once.txt").exists());
    assert!(
        b.state
            .devices
            .iter()
            .any(|d| d.fingerprint == a_id && !d.files_allowed)
    );

    let again = a_file_called("again.txt");
    a.drop_files(b_id, &[&again]).await;
    b.wait_for("being asked again", |s| s.incoming_files.is_some())
        .await;
    assert!(!b_desktop.join("again.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn turning_the_first_files_down_leaves_that_pc_not_allowed() {
    let (mut a, mut b, b_desktop) = two_new_to_each_other().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let unwanted = a_file_called("unwanted.txt");
    a.drop_files(b_id, &[&unwanted]).await;
    // Saying "always" along with no doesn't let it in.
    b.answer_files(false, true).await;
    let sent = a.wait_for_transfer_end().await;
    assert!(
        matches!(sent.stage, TransferStage::Failed { .. }),
        "{sent:?}"
    );
    assert!(!b_desktop.join("unwanted.txt").exists());
    assert!(
        b.state
            .devices
            .iter()
            .any(|d| d.fingerprint == a_id && !d.files_allowed)
    );
}

/// Dropped twice before anyone answered: allowing the PC answers for the second as well.
#[tokio::test(flavor = "multi_thread")]
async fn allowing_a_pc_lets_through_everything_else_it_had_waiting() {
    let (mut a, mut b, b_desktop) = two_new_to_each_other().await;
    let b_id = b.fingerprint();
    let (first, second) = (a_file_called("one.txt"), a_file_called("two.txt"));
    a.drop_files(b_id, &[&first]).await;
    b.wait_for("being asked", |s| s.incoming_files.is_some())
        .await;
    a.drop_files(b_id, &[&second]).await;
    let_things_settle().await;

    b.answer_files(true, true).await;
    b.wait_for("nothing more to ask", |s| {
        s.incoming_files.is_none()
            && s.transfers.len() == 2
            && s.transfers.iter().all(|t| t.stage.is_over())
    })
    .await;
    assert!(b_desktop.join("one.txt").exists());
    assert!(b_desktop.join("two.txt").exists());
}

/// A PC set to take files from anyone doesn't ask, or remember anyone for it.
#[tokio::test(flavor = "multi_thread")]
async fn saving_straight_away_asks_nobody() {
    let (mut a, mut b, b_desktop) = two_new_to_each_other().await;
    let b_id = b.fingerprint();
    b.set_rule(|rules| rules.receive_files = ReceiveFiles::AnyDevice)
        .await;
    let file = a_file_called("straight.txt");
    a.drop_files(b_id, &[&file]).await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert!(b_desktop.join("straight.txt").exists());
    assert!(b.state.devices.iter().all(|d| !d.files_allowed));
}

/// Asking every time overrides having been allowed before.
#[tokio::test(flavor = "multi_thread")]
async fn asking_every_time_asks_even_a_pc_that_was_allowed() {
    let (mut a, mut b, b_desktop) = two_new_to_each_other().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    b.request(Request::SetFilesAllowed {
        device: a_id,
        allowed: true,
    })
    .await;
    b.wait_for_files_allowed(a_id, true).await;
    b.set_rule(|rules| rules.receive_files = ReceiveFiles::Ask)
        .await;

    let file = a_file_called("asked.txt");
    a.drop_files(b_id, &[&file]).await;
    b.wait_for("being asked", |s| s.incoming_files.is_some())
        .await;
    assert!(!b_desktop.join("asked.txt").exists());
}

/// Forgetting a PC forgets that it was allowed, along with everything else about it.
#[tokio::test(flavor = "multi_thread")]
async fn a_pc_forgotten_and_paired_again_has_to_ask_again() {
    let (mut a, mut b, _) = two_new_to_each_other().await;
    let a_id = a.fingerprint();
    b.request(Request::SetFilesAllowed {
        device: a_id,
        allowed: true,
    })
    .await;
    b.wait_for_files_allowed(a_id, true).await;

    b.request(Request::Forget { device: a_id }).await;
    b.wait_for("forgotten", |s| s.devices.is_empty()).await;
    pair(&mut a, &mut b).await;
    assert!(b.state.devices.iter().all(|d| !d.files_allowed));
}

fn second_monitor(id: &str, x: f64) -> Monitor {
    Monitor {
        id: id.into(),
        name: "External".into(),
        bounds: Rect::new(x, 0.0, 1920.0, 1080.0),
        scale: 1.0,
        primary: false,
    }
}

fn position_of(state: &UiState, device: Fingerprint, monitor: &str) -> Option<(f64, f64)> {
    state
        .screens
        .iter()
        .find(|screen| screen.device == device && screen.monitor == monitor)
        .map(|screen| (screen.x, screen.y))
}

fn no_screens_overlap(state: &UiState) -> bool {
    state.screens.iter().enumerate().all(|(at, first)| {
        state.screens[at + 1..].iter().all(|second| {
            first.x + first.width <= second.x
                || second.x + second.width <= first.x
                || first.y + first.height <= second.y
                || second.y + second.height <= first.y
        })
    })
}

/// Arrange the screens, change them, and the arrangement is still there.
#[tokio::test(flavor = "multi_thread")]
async fn a_layout_comes_back_with_its_screens_even_after_a_restart() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.wait_for("the layout to be saved", |s| s.layout_saved)
        .await;

    // A second screen is plugged in, and arranged by hand.
    let alone = a.platform.monitors();
    let mut with_external = alone.clone();
    with_external.push(second_monitor("external", 1920.0));
    a.platform.set_monitors(with_external.clone());
    a.wait_for("the new screen", |s| {
        position_of(s, a_id, "external").is_some() && !s.layout_saved
    })
    .await;
    a.request(Request::SetPlacements {
        placements: vec![Placement {
            device: a_id,
            monitor: "external".into(),
            x: -1920.0,
            y: 0.0,
            width: None,
        }],
    })
    .await;
    a.wait_for("it to move", |s| {
        position_of(s, a_id, "external") == Some((-1920.0, 0.0)) && s.layout_saved
    })
    .await;
    let saved_layouts = a.state.saved_layouts;
    assert_eq!(saved_layouts, 2);

    // Unplugged again: that one goes, and the others are as they were.
    a.platform.set_monitors(alone);
    a.wait_for("the screen to go", |s| {
        position_of(s, a_id, "external").is_none()
    })
    .await;
    assert_eq!(position_of(&a.state, b_id, "fake-0"), Some((1920.0, 0.0)));
    assert!(
        a.state.layout_saved,
        "those screens have a layout of their own"
    );

    // Everything stops, and it's plugged in again while it's all off.
    let (dir, channel, platform) = a.stop_for_restart().await;
    platform.set_monitors(with_external);
    let a = Device::start_in("alpha", dir, channel, platform).await;
    assert_eq!(
        position_of(&a.state, a_id, "external"),
        Some((-1920.0, 0.0))
    );
    assert_eq!(position_of(&a.state, b_id, "fake-0"), Some((1920.0, 0.0)));
    assert!(a.state.layout_saved);
    assert_eq!(a.state.saved_layouts, saved_layouts);
}

/// Rearranging in Windows' display settings changes where a PC's screens are for Windows.
/// The canvas is what says where they are for Hackerman.
#[tokio::test(flavor = "multi_thread")]
async fn rearranging_the_screens_in_the_display_settings_leaves_the_canvas_alone() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let mut monitors = a.platform.monitors();
    monitors.push(second_monitor("external", 1920.0));
    a.platform.set_monitors(monitors.clone());
    a.wait_for("the new screen", |s| {
        position_of(s, a_id, "external").is_some()
    })
    .await;
    a.request(Request::SetPlacements {
        placements: vec![Placement {
            device: a_id,
            monitor: "external".into(),
            x: -1920.0,
            y: 0.0,
            width: None,
        }],
    })
    .await;
    a.wait_for("it to move", |s| {
        position_of(s, a_id, "external") == Some((-1920.0, 0.0))
    })
    .await;

    // Now Windows has the external one above the main one instead of beside it. Its
    // height changes as well, only so that there's a way to see this has been noticed.
    monitors[1].bounds = Rect::new(0.0, -1200.0, 1920.0, 1200.0);
    a.platform.set_monitors(monitors);
    a.wait_for("the change to be seen", |s| {
        s.screens
            .iter()
            .any(|screen| screen.monitor == "external" && screen.pixel_height == 1200.0)
    })
    .await;
    assert_eq!(
        position_of(&a.state, a_id, "external"),
        Some((-1920.0, 0.0))
    );
    assert_eq!(position_of(&a.state, b_id, "fake-0"), Some((1920.0, 0.0)));
    assert!(a.state.layout_saved, "it's the same screens");

    assert!(a.move_mouse(1919.0, 500.0), "the crossing still works");
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
}

/// Windows says the new screen is to the right, which on the canvas is the other PC.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_screen_never_lands_on_top_of_another_pc() {
    let (mut a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let mut monitors = a.platform.monitors();
    monitors.push(second_monitor("external", 1920.0));
    a.platform.set_monitors(monitors);

    for device in [&mut a, &mut b] {
        device
            .wait_for("the new screen", |s| {
                position_of(s, a_id, "external").is_some()
            })
            .await;
        assert!(
            no_screens_overlap(&device.state),
            "{:?}",
            device.state.screens
        );
        assert_eq!(
            position_of(&device.state, b_id, "fake-0"),
            Some((1920.0, 0.0)),
            "the other PC stays where it is"
        );
    }
    assert_eq!(
        position_of(&a.state, a_id, "external"),
        position_of(&b.state, a_id, "external"),
        "and both PCs see the same"
    );
}

/// A higher resolution makes a screen larger on the canvas, and into the one next to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_higher_resolution_keeps_the_crossing_and_going_back_restores_the_place() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    let usual = a.platform.monitors();
    let mut larger = usual.clone();
    larger[0].bounds = Rect::new(0.0, 0.0, 2560.0, 1080.0);
    a.platform.set_monitors(larger);
    a.wait_for("the other PC to make room", |s| {
        position_of(s, b_id, "fake-0") == Some((2560.0, 0.0))
    })
    .await;
    assert!(a.state.layout_saved, "it's the same screens");

    assert!(a.move_mouse(2559.0, 500.0), "it can still be crossed");
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;

    a.platform.set_monitors(usual);
    a.wait_for("the other PC to go back", |s| {
        position_of(s, b_id, "fake-0") == Some((1920.0, 0.0))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_a_layout_as_it_is_moves_nothing_and_reaches_the_other_pc() {
    let (mut a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let mut monitors = a.platform.monitors();
    monitors.push(second_monitor("external", 1920.0));
    a.platform.set_monitors(monitors);
    a.wait_for("the new screen", |s| {
        position_of(s, a_id, "external").is_some() && !s.layout_saved
    })
    .await;
    let before = a.state.screens.clone();

    a.request(Request::SetPlacements { placements: vec![] })
        .await;
    a.wait_for("it to be saved", |s| s.layout_saved).await;
    assert_eq!(a.state.screens, before);
    b.wait_for("the other PC to have it", |s| {
        s.layout_saved && position_of(s, a_id, "external").is_some()
    })
    .await;
    assert_eq!(
        position_of(&b.state, a_id, "external"),
        position_of(&a.state, a_id, "external")
    );
    assert_eq!(position_of(&a.state, b_id, "fake-0"), Some((1920.0, 0.0)));
}

#[tokio::test(flavor = "multi_thread")]
async fn turning_files_down_tells_the_sender_and_saves_nothing() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    b.set_rule(|rules| rules.receive_files = ReceiveFiles::Ask)
        .await;
    let source = temp_folder("source").join("unwanted.txt");
    std::fs::write(&source, "no").unwrap();

    a.drop_files(b_id, &[&source]).await;
    b.answer_files(false, false).await;

    let sent = a.wait_for_transfer_end().await;
    let TransferStage::Failed { reason } = &sent.stage else {
        panic!("{sent:?}");
    };
    assert!(reason.contains("turned"), "{reason}");
    assert!(!b_desktop.join("unwanted.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_set_to_refuse_files_saves_nothing() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    b.set_rule(|rules| rules.receive_files = ReceiveFiles::Never)
        .await;
    let source = temp_folder("source").join("refused.txt");
    std::fs::write(&source, "no").unwrap();

    a.drop_files(b_id, &[&source]).await;
    let sent = a.wait_for_transfer_end().await;
    assert!(
        matches!(sent.stage, TransferStage::Failed { .. }),
        "{sent:?}"
    );
    assert!(!b_desktop.join("refused.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_sent_while_either_pc_is_disabled() {
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let source = temp_folder("source").join("file.txt");
    std::fs::write(&source, "x").unwrap();

    b.turn(false).await;
    a.wait_until_status(b_id, DeviceStatus::Off).await;
    assert!(!a.state.devices.iter().any(|d| d.can_receive_files));
    a.drop_files(b_id, &[&source]).await;
    let refused = a.wait_for_transfer_end().await;
    assert!(
        matches!(refused.stage, TransferStage::Failed { .. }),
        "{refused:?}"
    );

    b.turn(true).await;
    a.wait_until_status(b_id, DeviceStatus::Connected).await;
    a.turn(false).await;
    b.wait_until_status(a_id, DeviceStatus::Off).await;
    a.drop_files(b_id, &[&source]).await;
    a.wait_for("refused here too", |s| {
        s.transfers.len() == 2 && s.transfers.iter().all(|t| t.stage.is_over())
    })
    .await;
    assert!(!b_desktop.join("file.txt").exists());
}

/// A large file that goes away with the test.
struct BigFile(PathBuf);

impl std::ops::Deref for BigFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for BigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Large enough to still be going, with progress showing on both sides.
async fn big_file(size: usize) -> BigFile {
    let source = temp_folder("source").join("big.bin");
    std::fs::write(&source, big_content(size)).unwrap();
    BigFile(source)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_file_shows_progress_on_both_ends_and_arrives_intact() {
    const SIZE: usize = 150 * 1024 * 1024;
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = big_file(SIZE).await;

    a.drop_files(b_id, &[&source]).await;
    a.wait_for("progress partway", |s| {
        s.transfers
            .last()
            .is_some_and(|t| t.done_bytes > 0 && t.done_bytes < t.total_bytes)
    })
    .await;
    b.wait_for("progress partway here too", |s| {
        s.transfers
            .last()
            .is_some_and(|t| t.done_bytes > 0 && t.stage == TransferStage::Running)
    })
    .await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Done, "{sent:?}");
    assert_eq!(sent.done_bytes, SIZE as u64);
    assert!(std::fs::read(b_desktop.join("big.bin")).unwrap() == big_content(SIZE));
    assert!(parts_left(&b_desktop).is_empty());
    let _ = std::fs::remove_file(b_desktop.join("big.bin"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sender_can_cancel_and_nothing_unfinished_is_left() {
    const SIZE: usize = 300 * 1024 * 1024;
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = big_file(SIZE).await;
    a.drop_files(b_id, &[&source]).await;
    b.wait_for("started", |s| {
        s.transfers.last().is_some_and(|t| t.done_bytes > 0)
    })
    .await;

    let id = a.state.transfers.last().unwrap().id.clone();
    a.request(Request::CancelTransfer { id }).await;
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Cancelled);
    let received = b.wait_for_transfer_end().await;
    assert!(received.stage.is_over());
    assert_ne!(received.stage, TransferStage::Done);
    assert!(!b_desktop.join("big.bin").exists());
    assert!(parts_left(&b_desktop).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_receiver_can_cancel_and_the_sender_hears() {
    const SIZE: usize = 300 * 1024 * 1024;
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = big_file(SIZE).await;
    a.drop_files(b_id, &[&source]).await;
    b.wait_for("started", |s| {
        s.transfers.last().is_some_and(|t| t.done_bytes > 0)
    })
    .await;

    let id = b.state.transfers.last().unwrap().id.clone();
    b.request(Request::CancelTransfer { id }).await;
    let received = b.wait_for_transfer_end().await;
    assert_eq!(received.stage, TransferStage::Cancelled);
    let sent = a.wait_for_transfer_end().await;
    assert_eq!(sent.stage, TransferStage::Cancelled);
    assert!(!b_desktop.join("big.bin").exists());
    assert!(parts_left(&b_desktop).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_partway_stops_it_and_says_so_on_both_ends() {
    const SIZE: usize = 300 * 1024 * 1024;
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = big_file(SIZE).await;
    a.drop_files(b_id, &[&source]).await;
    b.wait_for("started", |s| {
        s.transfers.last().is_some_and(|t| t.done_bytes > 0)
    })
    .await;

    b.turn(false).await;
    assert!(b.wait_for_transfer_end().await.stage.is_over());
    let sent = a.wait_for_transfer_end().await;
    assert_ne!(sent.stage, TransferStage::Done, "{sent:?}");
    assert!(parts_left(&b_desktop).is_empty());
}

/// Quitting mustn't get stuck waiting on a transfer, or leave half a file behind.
#[tokio::test(flavor = "multi_thread")]
async fn quitting_partway_stops_promptly_and_leaves_nothing_unfinished() {
    const SIZE: usize = 300 * 1024 * 1024;
    let (mut a, mut b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = big_file(SIZE).await;
    a.drop_files(b_id, &[&source]).await;
    b.wait_for("started", |s| {
        s.transfers.last().is_some_and(|t| t.done_bytes > 0)
    })
    .await;

    let started = Instant::now();
    b.stop_for_restart().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let gone = async {
        while !parts_left(&b_desktop).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), gone)
        .await
        .unwrap();
    assert!(!b_desktop.join("big.bin").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_cannot_be_read_is_reported_to_the_sender() {
    let (mut a, b, _, b_desktop) = two_with_desktops().await;
    let b_id = b.fingerprint();
    a.drop_files(b_id, &[&temp_folder("source").join("not there.txt")])
        .await;
    let sent = a.wait_for_transfer_end().await;
    assert!(
        matches!(sent.stage, TransferStage::Failed { .. }),
        "{sent:?}"
    );
    let on_desktop: Vec<_> = std::fs::read_dir(&b_desktop).unwrap().flatten().collect();
    assert!(on_desktop.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_transfers_can_be_dismissed_but_running_ones_cannot() {
    const SIZE: usize = 300 * 1024 * 1024;
    let (mut a, mut b, _, _) = two_with_desktops().await;
    let b_id = b.fingerprint();
    let source = big_file(SIZE).await;
    a.drop_files(b_id, &[&source]).await;
    b.wait_for("started", |s| {
        s.transfers.last().is_some_and(|t| t.done_bytes > 0)
    })
    .await;
    let id = a.state.transfers.last().unwrap().id.clone();
    a.request(Request::DismissTransfer { id: id.clone() }).await;
    let_things_settle().await;
    a.request(Request::CancelTransfer { id: id.clone() }).await;
    a.wait_for_transfer_end().await;

    a.request(Request::DismissTransfer { id }).await;
    a.wait_for("gone", |s| s.transfers.is_empty()).await;
}

/// A PC on 0.5.0 stays connected, and is simply not sent files to save.
#[tokio::test(flavor = "multi_thread")]
async fn a_pc_on_an_older_version_is_not_sent_files_to_save() {
    let (mut a, old) = device_with_hand_written_peer(FEATURE_FILES).await;
    let old_id = old.id;
    a.wait_until_status(old_id, DeviceStatus::Connected).await;
    let device = a
        .state
        .devices
        .iter()
        .find(|d| d.fingerprint == old_id)
        .unwrap();
    assert!(!device.can_receive_files);
    assert!(
        !device.needs_update_for_files,
        "it can still take the clipboard"
    );

    let source = temp_folder("source").join("file.txt");
    std::fs::write(&source, "x").unwrap();
    a.drop_files(old_id, &[&source]).await;
    let sent = a.wait_for_transfer_end().await;
    let TransferStage::Failed { reason } = &sent.stage else {
        panic!("{sent:?}");
    };
    assert!(reason.contains("Update"), "{reason}");

    let nothing = tokio::time::timeout(Duration::from_millis(300), old.blobs.receive()).await;
    assert!(nothing.is_err(), "nothing should have been sent to it");
    assert_eq!(status_of(&a.state, old_id), Some(DeviceStatus::Connected));
}

/// Names from another computer decide where files go on this one.
#[tokio::test(flavor = "multi_thread")]
async fn files_sent_with_names_that_could_escape_are_refused() {
    let desktop = temp_folder("desktop");
    let (mut a, mut peer) = device_with_hand_written_peer(FEATURES).await;
    a.platform.set_desktop_folder(desktop.clone());
    let peer_id = peer.id;
    let id = ClipVersion {
        counter: 7,
        owner: peer_id,
    };
    let blob = |path: &str, id| ClipboardBlob {
        version: id,
        data: ClipboardData::default(),
        files: vec![SharedFile {
            path: path.into(),
            size: 1,
            is_dir: false,
            modified_ms: None,
        }],
        save_to_folder: true,
    };
    peer.blobs
        .send(&hk_proto::encode_clipboard(&blob("../../evil.exe", id)).unwrap())
        .await
        .unwrap();
    let answer = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Message::Transfer(answered, update) = peer.receiver.recv().await.unwrap() {
                return (answered, update);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(answer.0, id);
    assert!(matches!(answer.1, TransferUpdate::Failed(_)), "{answer:?}");
    a.wait_for("no transfer", |s| s.transfers.is_empty()).await;
    assert!(!desktop.parent().unwrap().join("evil.exe").exists());
}

/// A device can only offer its own files, not pose as another one's.
#[tokio::test(flavor = "multi_thread")]
async fn files_offered_in_the_name_of_another_device_are_ignored() {
    let (a, peer) = device_with_hand_written_peer(FEATURES).await;
    let someone_else = ClipVersion {
        counter: 7,
        owner: Fingerprint::from_bytes([9; 32]),
    };
    let blob = ClipboardBlob {
        version: someone_else,
        data: ClipboardData::default(),
        files: vec![SharedFile {
            path: "file.txt".into(),
            size: 1,
            is_dir: false,
            modified_ms: None,
        }],
        save_to_folder: true,
    };
    peer.blobs
        .send(&hk_proto::encode_clipboard(&blob).unwrap())
        .await
        .unwrap();
    let_things_settle().await;
    assert!(a.state.transfers.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_new_settings_survive_a_restart() {
    let mut a = Device::start("alpha").await;
    a.set_rule(|rules| {
        rules.cross_modifier = CrossModifier::Shift;
        rules.corner_guard = 50;
        rules.receive_files = ReceiveFiles::Ask;
        rules.drop_folder = Some("D:\\Reçus".to_string());
        rules.name_clash = NameClash::Replace;
    })
    .await;
    let wanted = a.state.rules.clone();
    let (dir, channel, platform) = a.stop_for_restart().await;
    let a = Device::start_in("alpha", dir, channel, platform).await;
    assert_eq!(a.state.rules, wanted);
}

/// The whole way from the setting to the cursor.
#[tokio::test(flavor = "multi_thread")]
async fn the_cross_settings_apply_to_the_real_cursor() {
    let (mut a, b) = side_by_side().await;
    a.set_rule(|rules| {
        rules.cross_modifier = CrossModifier::Ctrl;
        rules.corner_guard = 50;
    })
    .await;
    assert!(!a.move_mouse(1919.0, 500.0), "the key isn't held");
    assert!(!a.press(KeyCode::ControlLeft, true));
    assert!(!a.move_mouse(1919.0, 10.0), "too near the corner");
    assert!(a.move_mouse(1919.0, 500.0));
    wait_for_injected(&b, InputEvent::PointerPosition { x: 0.0, y: 500.0 }).await;
}

// What's open, and what's playing

fn window_of(id: &str, name: &str, monitors: &[&str]) -> RunningApp {
    RunningApp {
        id: id.into(),
        name: name.into(),
        monitors: monitors.iter().map(ToString::to_string).collect(),
        minimized: false,
        media: None,
    }
}

fn player(id: &str, playing: Option<bool>) -> RunningApp {
    RunningApp {
        media: Some(MediaState {
            playing,
            can_go_back: true,
            can_go_forward: true,
        }),
        ..window_of(id, id, &["fake-0"])
    }
}

/// What a device's screen shows of apps, from where this state was made.
fn apps_on_screen<'a>(state: &'a UiState, device: Fingerprint, monitor: &str) -> Vec<&'a str> {
    state
        .screens
        .iter()
        .find(|screen| screen.device == device && screen.monitor == monitor)
        .map(|screen| screen.apps.iter().map(|app| app.id.as_str()).collect())
        .unwrap_or_default()
}

/// Whether a device's screen shows something playing, or that isn't, from this state.
fn shows_playing(state: &UiState, device: Fingerprint, app: &str, playing: bool) -> bool {
    state.screens.iter().any(|screen| {
        screen.device == device
            && screen.apps.iter().any(|shown| {
                shown.id == app
                    && shown
                        .media
                        .is_some_and(|media| media.playing == Some(playing))
            })
    })
}

/// Waits until this device has asked this app here to do this.
async fn wait_for_media_command(device: &Device, app: &str, command: MediaCommand) {
    let wanted = (app.to_string(), command);
    let wait = async {
        while !device.platform.media_commands().contains(&wanted) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{} never got {wanted:?}, only {:?}",
                device.name,
                device.platform.media_commands()
            )
        });
}

/// Holds Ctrl and the left Alt on this device's keyboard, and presses a key.
fn press_with_ctrl_alt(device: &Device, code: KeyCode) -> bool {
    device.press(KeyCode::ControlLeft, true);
    device.press(KeyCode::AltLeft, true);
    let hidden = device.press(code, true);
    device.press(code, false);
    device.press(KeyCode::AltLeft, false);
    device.press(KeyCode::ControlLeft, false);
    hidden
}

/// An agent paired with a device that isn't there, for a test to play the part of.
async fn with_a_paired_device_to_play(name: &str) -> (Device, Identity, Fingerprint) {
    let identity = Identity::generate().unwrap();
    let id = unique_id();
    let dir = fresh_dir(id);
    std::fs::write(
        dir.join("devices.json"),
        format!(
            r#"[{{"fingerprint":"{}","name":"{name}"}}]"#,
            identity.fingerprint()
        ),
    )
    .unwrap();
    let a = Device::start_in(
        "alpha",
        dir,
        channel_name(id),
        FakePlatform::with_screens(1),
    )
    .await;
    let a_id = a.fingerprint();
    (a, identity, a_id)
}

fn hello_from(name: &str, features: u32, monitors: Vec<Monitor>) -> Message {
    Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        device_name: name.into(),
        monitors,
        layout: LayoutDoc::default(),
        model: None,
        enabled: true,
        features,
    }
}

/// The apps on each PC, on the drawing of each of its screens.
#[tokio::test(flavor = "multi_thread")]
async fn each_screen_shows_the_apps_open_on_it_wherever_it_is_seen_from() {
    let monitors = vec![
        monitor("left", 0.0, 0.0, 1920.0, 1080.0, 1.0),
        monitor("right", 1920.0, 0.0, 1920.0, 1080.0, 1.0),
    ];
    let b_platform = FakePlatform::new(monitors);
    b_platform.set_running_apps(vec![
        window_of("code", "Visual Studio Code", &["left"]),
        window_of("msedge", "Microsoft Edge", &["right", "left"]),
    ]);
    let mut a = Device::start("alpha").await;
    let mut b = Device::start_with("bravo", b_platform).await;
    let b_id = b.fingerprint();
    pair(&mut a, &mut b).await;

    a.wait_for("the other PC's apps", |s| {
        apps_on_screen(s, b_id, "left") == ["code", "msedge"]
            && apps_on_screen(s, b_id, "right") == ["msedge"]
    })
    .await;
    b.wait_for("its own apps", |s| {
        apps_on_screen(s, b_id, "left") == ["code", "msedge"]
    })
    .await;

    // A window moves to the other screen, and another app closes.
    b.platform
        .set_running_apps(vec![window_of("code", "Visual Studio Code", &["right"])]);
    a.wait_for("the change", |s| {
        apps_on_screen(s, b_id, "left").is_empty() && apps_on_screen(s, b_id, "right") == ["code"]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn something_playing_with_no_window_shows_on_the_main_screen() {
    let monitors = vec![
        monitor("main", 0.0, 0.0, 1920.0, 1080.0, 1.0),
        monitor("other", 1920.0, 0.0, 1920.0, 1080.0, 1.0),
    ];
    let b_platform = FakePlatform::new(monitors);
    b_platform.set_running_apps(vec![RunningApp {
        monitors: vec![],
        ..player("spotify", Some(true))
    }]);
    let mut a = Device::start("alpha").await;
    let mut b = Device::start_with("bravo", b_platform).await;
    let b_id = b.fingerprint();
    pair(&mut a, &mut b).await;

    a.wait_for("it on the main screen", |s| {
        apps_on_screen(s, b_id, "main") == ["spotify"]
            && apps_on_screen(s, b_id, "other").is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pcs_apps_go_when_it_does() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![window_of("code", "Code", &["fake-0"])]);
    a.wait_for("its apps", |s| {
        apps_on_screen(s, b_id, "fake-0") == ["code"]
    })
    .await;

    b.stop().await;
    a.wait_for("them gone", |s| {
        s.devices
            .iter()
            .any(|d| d.fingerprint == b_id && d.status == DeviceStatus::Offline)
            && apps_on_screen(s, b_id, "fake-0").is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_picture_of_each_app_is_looked_for_once_and_reaches_the_other_pc() {
    let b_platform = FakePlatform::with_screens(1);
    b_platform.set_app_icon("code", b"\x89PNG and the rest".to_vec());
    b_platform.set_running_apps(vec![
        window_of("code", "Code", &["fake-0"]),
        window_of("no-picture", "No picture", &["fake-0"]),
    ]);
    let mut a = Device::start("alpha").await;
    let mut b = Device::start_with("bravo", b_platform).await;
    let b_id = b.fingerprint();
    pair(&mut a, &mut b).await;

    let shown_on = |state: &UiState, id: &str| {
        state
            .screens
            .iter()
            .filter(|screen| screen.device == b_id)
            .flat_map(|screen| &screen.apps)
            .find(|app| app.id == id)
            .and_then(|app| app.icon.clone())
    };
    a.wait_for("the picture", |s| shown_on(s, "code").is_some())
        .await;
    let shown = shown_on(&a.state, "code").unwrap();
    assert!(shown.starts_with("data:image/png;base64,"), "{shown}");
    assert_eq!(shown_on(&a.state, "no-picture"), None);

    // Several looks later, only a new app is any more work.
    b.platform.set_running_apps(vec![
        window_of("code", "Code", &["fake-0"]),
        window_of("new", "New", &["fake-0"]),
    ]);
    a.wait_for("the new app", |s| {
        apps_on_screen(s, b_id, "fake-0") == ["code", "new"]
    })
    .await;
    let mut asked = b.platform.icons_asked_for();
    asked.sort();
    assert_eq!(asked, ["code", "new", "no-picture"]);
}

/// A device could say anything about an app, and this goes straight into the page.
#[tokio::test(flavor = "multi_thread")]
async fn what_another_pc_says_about_its_apps_is_kept_in_bounds() {
    let (mut a, identity, a_id) = with_a_paired_device_to_play("other-pc").await;
    let other_id = identity.fingerprint();
    let endpoint = Endpoint::bind(
        &identity,
        "127.0.0.1:0".parse().unwrap(),
        TrustStore::new([a_id]),
    )
    .unwrap();
    let link = endpoint
        .connect(a.address().parse().unwrap(), a_id)
        .await
        .unwrap();
    let (mut sender, _receiver) = link.split();
    let screen = monitor("m", 0.0, 0.0, 800.0, 600.0, 1.0);
    sender
        .send(&hello_from("other-pc", FEATURES, vec![screen]))
        .await
        .unwrap();
    a.wait_connected_to(other_id).await;

    let long = "x".repeat(10_000);
    sender
        .send(&Message::AppIcon {
            app: "script".into(),
            png: b"<svg onload=alert(1)>".to_vec(),
        })
        .await
        .unwrap();
    sender
        .send(&Message::AppIcon {
            app: "huge".into(),
            png: [b"\x89PNG".as_slice(), &[0; 100_000]].concat(),
        })
        .await
        .unwrap();
    let mut too_many = vec![
        window_of(&long, &long, &["m"]),
        window_of("script", "Script", &["m"]),
        window_of("huge", "Huge", &["m"]),
    ];
    too_many.extend((0..500).map(|i| window_of(&format!("app{i}"), "App", &["m"])));
    sender.send(&Message::Apps(too_many)).await.unwrap();

    a.wait_for("the list", |s| !apps_on_screen(s, other_id, "m").is_empty())
        .await;
    let screen = a
        .state
        .screens
        .iter()
        .find(|screen| screen.device == other_id)
        .unwrap();
    assert_eq!(screen.apps.len(), hk_core::apps::MAX_APPS);
    assert_eq!(screen.apps[0].id.len(), hk_core::apps::MAX_APP_TEXT);
    assert_eq!(screen.apps[0].name.len(), hk_core::apps::MAX_APP_TEXT);
    assert!(screen.apps.iter().all(|app| app.icon.is_none()));
}

/// Copies of Hackerman from before this have no use for these, and the oldest drop the
/// connection on a message they don't know. These are what 0.8.0 says it can do.
#[tokio::test(flavor = "multi_thread")]
async fn a_pc_on_an_older_version_hears_nothing_about_apps() {
    let (a, identity, a_id) = with_a_paired_device_to_play("older").await;
    a.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.platform.set_app_icon("spotify", b"\x89PNG".to_vec());
    let endpoint = Endpoint::bind(
        &identity,
        "127.0.0.1:0".parse().unwrap(),
        TrustStore::new([a_id]),
    )
    .unwrap();
    let link = endpoint
        .connect(a.address().parse().unwrap(), a_id)
        .await
        .unwrap();
    let (mut sender, mut receiver) = link.split();
    let screen = monitor("m", 0.0, 0.0, 800.0, 600.0, 1.0);
    sender
        .send(&hello_from(
            "older",
            FEATURE_FILES | FEATURE_SEND_FILES,
            vec![screen],
        ))
        .await
        .unwrap();

    // Long enough to have looked at the apps more than once, with the connection up.
    let heard = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match receiver.recv().await {
                Ok(Message::Apps(_) | Message::AppIcon { .. } | Message::Media { .. }) => {
                    return true;
                }
                Ok(_) => {}
                Err(error) => panic!("the connection went: {error}"),
            }
        }
    })
    .await;
    assert!(heard.is_err(), "it should have heard nothing about apps");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_can_keep_its_apps_to_itself() {
    let (mut a, mut b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![window_of("code", "Code", &["fake-0"])]);
    a.wait_for("its apps", |s| {
        apps_on_screen(s, b_id, "fake-0") == ["code"]
    })
    .await;

    b.request(Request::SetRules {
        rules: Rules {
            share_apps: false,
            ..Rules::default()
        },
    })
    .await;
    a.wait_for("them gone from the other PC", |s| {
        apps_on_screen(s, b_id, "fake-0").is_empty()
    })
    .await;
    b.wait_for("them still showing on its own", |s| {
        apps_on_screen(s, b_id, "fake-0") == ["code"]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_that_turns_sharing_off_stops_saying_whats_open() {
    let (mut a, mut b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![window_of("code", "Code", &["fake-0"])]);
    a.wait_for("its apps", |s| {
        apps_on_screen(s, b_id, "fake-0") == ["code"]
    })
    .await;
    b.request(Request::SetEnabled { enabled: false }).await;
    a.wait_for("them gone", |s| {
        apps_on_screen(s, b_id, "fake-0").is_empty()
    })
    .await;
}

/// The buttons on the drawing of a screen.
#[tokio::test(flavor = "multi_thread")]
async fn the_buttons_on_a_screen_reach_the_player_on_that_pc() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("the player", |s| shows_playing(s, b_id, "spotify", true))
        .await;

    for command in [
        MediaCommand::PlayPause,
        MediaCommand::Back,
        MediaCommand::Forward,
    ] {
        a.request(Request::MediaCommand {
            device: b_id,
            app: "spotify".into(),
            command,
        })
        .await;
        wait_for_media_command(&b, "spotify", command).await;
    }
    assert!(a.platform.media_commands().is_empty());

    // And this PC's own, without leaving it.
    a.platform.set_running_apps(vec![player("stremio", None)]);
    a.request(Request::MediaCommand {
        device: a_id,
        app: "stremio".into(),
        command: MediaCommand::PlayPause,
    })
    .await;
    wait_for_media_command(&a, "stremio", MediaCommand::PlayPause).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_on_the_player_soon_shows_on_the_other_pc() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("playing", |s| shows_playing(s, b_id, "spotify", true))
        .await;

    // The button is pressed, and the player has paused by the time it's looked at.
    b.platform
        .set_running_apps(vec![player("spotify", Some(false))]);
    a.request(Request::MediaCommand {
        device: b_id,
        app: "spotify".into(),
        command: MediaCommand::PlayPause,
    })
    .await;
    a.wait_for("paused", |s| shows_playing(s, b_id, "spotify", false))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_can_refuse_to_have_its_media_controlled() {
    let (mut a, mut b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("the player", |s| shows_playing(s, b_id, "spotify", true))
        .await;

    b.request(Request::SetRules {
        rules: Rules {
            media_control: false,
            ..Rules::default()
        },
    })
    .await;
    // The app's still shown, but nothing says it can be controlled.
    a.wait_for("no controls", |s| {
        apps_on_screen(s, b_id, "fake-0") == ["spotify"] && !shows_playing(s, b_id, "spotify", true)
    })
    .await;

    // Asking anyway does nothing.
    a.request(Request::MediaCommand {
        device: b_id,
        app: "spotify".into(),
        command: MediaCommand::PlayPause,
    })
    .await;
    let_things_settle().await;
    assert!(b.platform.media_commands().is_empty());
}

/// Only what's listed as a player can be asked to do anything.
#[tokio::test(flavor = "multi_thread")]
async fn another_pc_cant_send_commands_to_just_anything() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform.set_running_apps(vec![
        player("spotify", Some(true)),
        window_of("notepad", "Notepad", &["fake-0"]),
    ]);
    a.wait_for("the player", |s| shows_playing(s, b_id, "spotify", true))
        .await;
    for app in ["notepad", "something-not-running"] {
        a.request(Request::MediaCommand {
            device: b_id,
            app: app.into(),
            command: MediaCommand::PlayPause,
        })
        .await;
    }
    let_things_settle().await;
    assert!(b.platform.media_commands().is_empty());
}

/// A shortcut for what's playing on whichever PC that is.
#[tokio::test(flavor = "multi_thread")]
async fn a_media_shortcut_reaches_whatever_is_playing_on_another_pc() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("the player", |s| shows_playing(s, b_id, "spotify", true))
        .await;

    assert!(
        press_with_ctrl_alt(&a, KeyCode::Space),
        "it's hidden from this PC"
    );
    wait_for_media_command(&b, "spotify", MediaCommand::PlayPause).await;
    assert!(press_with_ctrl_alt(&a, KeyCode::PageDown));
    wait_for_media_command(&b, "spotify", MediaCommand::Forward).await;
    assert!(press_with_ctrl_alt(&a, KeyCode::PageUp));
    wait_for_media_command(&b, "spotify", MediaCommand::Back).await;
    assert!(a.platform.media_commands().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_nothing_to_play_a_media_shortcut_stays_with_the_program_in_front() {
    let (a, b) = side_by_side().await;
    b.platform
        .set_running_apps(vec![window_of("notepad", "Notepad", &["fake-0"])]);
    tokio::time::sleep(TICK_AND_A_BIT).await;
    assert!(!press_with_ctrl_alt(&a, KeyCode::Space));
}

#[tokio::test(flavor = "multi_thread")]
async fn media_shortcuts_can_be_chosen_and_turned_off() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("the player", |s| shows_playing(s, b_id, "spotify", true))
        .await;

    let mut rules = Rules::default();
    rules.media_shortcuts.play_pause = Shortcut {
        key: Some(KeyCode::F13),
        ..Shortcut::default()
    };
    a.request(Request::SetRules {
        rules: rules.clone(),
    })
    .await;
    a.wait_for("the rules", |s| s.rules == rules).await;

    assert!(!press_with_ctrl_alt(&a, KeyCode::Space), "not any more");
    assert!(a.press(KeyCode::F13, true));
    a.press(KeyCode::F13, false);
    wait_for_media_command(&b, "spotify", MediaCommand::PlayPause).await;

    rules.media_shortcuts.enabled = false;
    a.request(Request::SetRules {
        rules: rules.clone(),
    })
    .await;
    a.wait_for("the rules", |s| s.rules == rules).await;
    assert!(!a.press(KeyCode::F13, true));
}

/// Pausing leaves nothing playing, and the same shortcut has to start it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_media_shortcut_goes_to_what_started_last_then_to_what_it_last_paused() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    let command_count = |device: &Device, app: &str| {
        device
            .platform
            .media_commands()
            .iter()
            .filter(|(id, _)| id == app)
            .count()
    };

    // Something's paused here, and something starts playing on the other PC.
    a.platform
        .set_running_apps(vec![player("msedge", Some(false))]);
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("both", |s| {
        shows_playing(s, a_id, "msedge", false) && shows_playing(s, b_id, "spotify", true)
    })
    .await;
    assert!(press_with_ctrl_alt(&a, KeyCode::Space));
    wait_for_media_command(&b, "spotify", MediaCommand::PlayPause).await;
    assert_eq!(command_count(&a, "msedge"), 0);

    // It pauses, leaving nothing playing anywhere: the next press starts it again, rather
    // than starting what's paused here.
    b.platform
        .set_running_apps(vec![player("spotify", Some(false))]);
    a.wait_for("paused", |s| shows_playing(s, b_id, "spotify", false))
        .await;
    assert!(press_with_ctrl_alt(&a, KeyCode::Space));
    let wait = async {
        while command_count(&b, "spotify") < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, wait).await.unwrap();
    assert_eq!(command_count(&a, "msedge"), 0);

    // Then something starts here too, later than the other. That's what it's for now.
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("playing again", |s| shows_playing(s, b_id, "spotify", true))
        .await;
    a.platform
        .set_running_apps(vec![player("msedge", Some(true))]);
    a.wait_for("both playing", |s| {
        shows_playing(s, a_id, "msedge", true) && shows_playing(s, b_id, "spotify", true)
    })
    .await;
    assert!(press_with_ctrl_alt(&a, KeyCode::Space));
    wait_for_media_command(&a, "msedge", MediaCommand::PlayPause).await;
    assert_eq!(command_count(&b, "spotify"), 2);
}

/// Stremio doesn't say whether it's playing, so it's the best guess with nothing else to go on.
#[tokio::test(flavor = "multi_thread")]
async fn a_player_that_doesnt_say_comes_before_something_known_to_be_paused() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.platform
        .set_running_apps(vec![player("msedge", Some(false))]);
    b.platform.set_running_apps(vec![player("stremio", None)]);
    a.wait_for("both", |s| {
        shows_playing(s, a_id, "msedge", false) && apps_on_screen(s, b_id, "fake-0") == ["stremio"]
    })
    .await;
    assert!(press_with_ctrl_alt(&a, KeyCode::Space));
    wait_for_media_command(&b, "stremio", MediaCommand::PlayPause).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_media_shortcut_leaves_the_cursor_where_it_is() {
    let (mut a, b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("the player", |s| shows_playing(s, a_id, "spotify", true))
        .await;
    a.move_mouse(100.0, 100.0);
    assert!(a.move_mouse(1919.0, 500.0));
    a.wait_for("on the other PC", |s| {
        s.control == ControlView::Controlling { device: b_id }
    })
    .await;

    assert!(press_with_ctrl_alt(&a, KeyCode::Space));
    wait_for_media_command(&a, "spotify", MediaCommand::PlayPause).await;
    assert_eq!(
        a.state.control,
        ControlView::Controlling { device: b_id },
        "it stays there"
    );
    // The other PC saw Ctrl and Alt go down and up, but not the key between them.
    assert!(!b.platform.injected().iter().any(|event| matches!(
        event,
        InputEvent::Key {
            code: KeyCode::Space,
            ..
        }
    )));
}

/// A locked PC says nothing about what's open, and isn't to be controlled.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_pc_says_nothing_about_its_apps_and_cant_be_asked_to_do_anything() {
    let (a, mut b) = side_by_side().await;
    let (a_id, b_id) = (a.fingerprint(), b.fingerprint());
    a.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    b.wait_for("the player", |s| shows_playing(s, a_id, "spotify", true))
        .await;

    a.platform.set_should_step_aside(true);
    let (dir, _, _) = a.wait_until_it_steps_aside().await;
    b.wait_for("it gone", |s| apps_on_screen(s, a_id, "fake-0").is_empty())
        .await;

    let helper = Device::start_lock_screen_helper("alpha", &dir, &[b_id]).await;
    helper
        .platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    b.wait_connected_to(a_id).await;

    b.request(Request::MediaCommand {
        device: a_id,
        app: "spotify".into(),
        command: MediaCommand::PlayPause,
    })
    .await;
    tokio::time::sleep(TICK_AND_A_BIT).await;
    b.request(Request::MediaCommand {
        device: a_id,
        app: "spotify".into(),
        command: MediaCommand::PlayPause,
    })
    .await;
    let_things_settle().await;
    assert!(apps_on_screen(&b.state, a_id, "fake-0").is_empty());
    assert!(helper.platform.media_commands().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn other_programs_sharing_the_keyboard_and_mouse_are_pointed_out() {
    let platform = FakePlatform::with_screens(1);
    platform.set_rival_programs(vec!["Input Director".into()]);
    let mut a = Device::start_with("alpha", platform).await;
    a.wait_for("them pointed out", |s| {
        s.rival_programs == ["Input Director"]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shortcuts_do_nothing_while_a_new_one_is_being_chosen() {
    let (mut a, b) = side_by_side().await;
    let b_id = b.fingerprint();
    b.platform
        .set_running_apps(vec![player("spotify", Some(true))]);
    a.wait_for("the player", |s| shows_playing(s, b_id, "spotify", true))
        .await;

    a.request(Request::RecordingShortcut { active: true }).await;
    let_things_settle().await;
    assert!(!press_with_ctrl_alt(&a, KeyCode::Space));
    assert!(!press_with_ctrl_alt(&a, KeyCode::Digit2));
    assert_eq!(a.state.control, ControlView::Home);
    assert!(b.platform.media_commands().is_empty());

    a.request(Request::RecordingShortcut { active: false })
        .await;
    let_things_settle().await;
    assert!(press_with_ctrl_alt(&a, KeyCode::Space));
    wait_for_media_command(&b, "spotify", MediaCommand::PlayPause).await;
}
