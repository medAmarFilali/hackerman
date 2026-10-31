//! The agent's main loop.
//!
//! All state lives here and changes in one place: other tasks (network
//! sessions, the app's connections, discovery, timers) only send [`Event`]s.
//! That avoids locks and makes the order of things easy to reason about.
//! The one exception is input, which is too time-critical to pass through here;
//! see [`crate::shared`].

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine as _;
use hk_core::apps::{self, AppAction, MAX_ICON_BYTES, MAX_ICONS, MediaCommand, RunningApp};
use hk_core::clipboard::{ClipVersion, ClipboardData, MAX_SHARED_FILES, SharedFile};
use hk_core::device::Fingerprint;
use hk_core::layout::{DeviceMonitors, LayoutDoc, Monitor, Placement, Topology};
use hk_core::rules::ReceiveFiles;
use hk_core::rules::Rules;
use hk_core::switcher::Switcher;
use hk_ipc::{
    AppView, ControlView, DeviceStatus, DeviceView, FromAgent, IncomingFilesView, IpcConnection,
    IpcListener, PairingStage, PairingView, PictureAnswer, Request, ScreenAsk, ScreenPicture,
    ScreenView, ThisDevice, TransferStage, TransferView, UiState,
};
use hk_net::{
    Accepted, BlobChannel, DataWriter, Discovery, DiscoveryEvent, Endpoint, FoundDevice, Identity,
    Link, MAX_BLOB_LEN, PairingChannel, TrustStore, generate_code, local_addresses,
};
use hk_platform::{Capture, FileSource, Platform};
use hk_proto::{
    ClipboardBlob, FEATURE_APP_ACTIONS, FEATURE_APPS, FEATURE_FILES, FEATURE_MEDIA,
    FEATURE_SCREENS, FEATURE_SEND_FILES, FEATURES, Message, PROTOCOL_VERSION, ScreenAnswer,
    ScreenRequest, TransferUpdate,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

use crate::clipboard::ClipboardSync;
use crate::config::{self, PairedDevice, Settings};
use crate::files::{
    self, Links, LocalFiles, OfferedFiles, RemoteFiles, SaveError, TransferControl,
};
use crate::pairing::{self, Paired};
use crate::screens;
use crate::session;
use crate::shared::Shared;

/// How often to check the monitors and retry reaching paired devices.
const TICK: Duration = Duration::from_secs(2);
/// When both devices could dial each other, the one with the lower fingerprint
/// goes first. The other only tries every few ticks, in case a firewall means
/// the connection only works in its direction.
const HIGHER_DIALS_EVERY_N_TICKS: u64 = 3;
/// How long the device showing the code refuses new attempts after a failure.
/// One wrong guess costs an attacker a new code, and this keeps them slow.
const PAIRING_COOLDOWN: Duration = Duration::from_secs(3);
/// A peer reporting more monitors than this is misbehaving.
const MAX_MONITORS: usize = 64;
/// Text from another device about a transfer is cut off at this many characters.
const MAX_REPORTED_TEXT: usize = 1024;
/// A make and model longer than this comes from a device that's misbehaving.
const MAX_MODEL_LENGTH: usize = 80;
/// How often to check for a full-screen app, when that rule is on.
const FULLSCREEN_CHECK: Duration = Duration::from_millis(500);
/// How often to check whether to hand this computer over, as the screen has locked or unlocked.
const LOCK_SCREEN_CHECK: Duration = Duration::from_millis(500);
/// How often to look at how transfers are going, and tell the other end.
const TRANSFER_CHECK: Duration = Duration::from_millis(250);
/// How long files being sent wait for a lost connection to come back. The other end is
/// also waiting, and it tells this end if it gives up, if it can.
const TRANSFER_LOST_AFTER: Duration = Duration::from_secs(60);
/// How many finished transfers stay on show.
const FINISHED_TRANSFERS_KEPT: usize = 20;
/// A device running another version keeps being refused, so don't try often. An
/// update on either side is still noticed quickly enough.
const INCOMPATIBLE_DIALS_EVERY_N_TICKS: u64 = 15;
/// How often to look for other programs that share the keyboard and mouse.
const RIVALS_CHECK_EVERY_N_TICKS: u64 = 30;
/// A tick this late means the agent was held up, or the computer was asleep.
const HELD_UP_AFTER: Duration = Duration::from_secs(5);
/// How long a player takes to change what it says it's doing, after being asked to.
const MEDIA_SETTLES: Duration = Duration::from_millis(300);
/// A device that asked for a picture this recently counts as looking at this one's screens. It's
/// long enough for pictures that come a few seconds apart.
const WATCHING_FOR: Duration = Duration::from_secs(10);

pub struct Options {
    /// Where to keep settings, the identity and saved data.
    pub config_dir: PathBuf,
    /// Overrides the port in the settings. Zero picks any free port.
    pub port: Option<u16>,
    /// The local channel the app connects to.
    pub channel: String,
    /// Whether to capture this computer's input.
    pub capture: bool,
    /// Whether to announce this device and look for others on the network.
    pub discovery: bool,
    /// Accept pairing with no app open, logging the code instead of showing it.
    /// Only for simulated devices used in development.
    pub headless_pairing: bool,
}

/// Something that happened, for the main loop to act on.
pub enum Event {
    AppConnected(UnboundedSender<FromAgent>),
    AppRequest(Request),
    /// The app wants a picture of a screen, here or on another device.
    ScreenAsked {
        id: u64,
        ask: ScreenAsk,
        reply: UnboundedSender<FromAgent>,
    },
    /// Another device wants a picture of a screen here.
    ScreenRequested {
        peer: Fingerprint,
        request: ScreenRequest,
        writer: DataWriter,
    },
    Discovery(DiscoveryEvent),
    Incoming(Box<Accepted>),
    Dialed {
        device: Fingerprint,
        result: Result<Link, hk_net::Error>,
    },
    SessionUp {
        id: u64,
        peer: Fingerprint,
        name: String,
        monitors: Vec<Monitor>,
        layout: LayoutDoc,
        model: Option<String>,
        enabled: bool,
        features: u32,
    },
    SessionDown {
        id: u64,
        peer: Fingerprint,
    },
    PeerMonitors {
        peer: Fingerprint,
        monitors: Vec<Monitor>,
    },
    PeerLayout {
        layout: LayoutDoc,
    },
    PeerRenamed {
        peer: Fingerprint,
        name: String,
    },
    PeerEnabled {
        peer: Fingerprint,
        enabled: bool,
    },
    ControlledBy {
        peer: Fingerprint,
        active: bool,
    },
    /// A device speaks a different version of the protocol.
    PeerIncompatible {
        peer: Fingerprint,
    },
    /// Which device this one controls changed, or something else the app shows.
    ControlChanged,
    /// Something new was copied on this device.
    ClipboardChanged,
    ClipboardOffered {
        peer: Fingerprint,
        version: ClipVersion,
    },
    ClipboardRequested {
        peer: Fingerprint,
        version: ClipVersion,
    },
    ClipboardArrived {
        peer: Fingerprint,
        bytes: Vec<u8>,
    },
    /// Content from another device is now on this device's clipboard.
    ClipboardApplied {
        version: ClipVersion,
    },
    /// Input stayed on this device, after a request to be told.
    LocalInput,
    /// Files to be sent have been gone through.
    FilesListed {
        id: ClipVersion,
        files: Vec<SharedFile>,
        paths: Vec<PathBuf>,
    },
    /// A device wants to send files to be saved here.
    FilesOffered {
        peer: Fingerprint,
        id: ClipVersion,
        files: Vec<SharedFile>,
    },
    /// Files have finished being saved here, for better or worse.
    FilesSaved {
        id: ClipVersion,
        result: Result<(), SaveError>,
    },
    /// News of files sent from here.
    TransferUpdated {
        peer: Fingerprint,
        id: ClipVersion,
        update: TransferUpdate,
    },
    /// A device no longer wants files saved here.
    TransferCancelled {
        peer: Fingerprint,
        id: ClipVersion,
    },
    /// What's open here, as just looked up.
    AppsRead {
        apps: Vec<RunningApp>,
        /// Pictures of apps seen for the first time. Not all of them have one.
        icons: Vec<(String, Vec<u8>)>,
        /// Which apps have now had their pictures looked for.
        looked_for: Vec<String>,
        /// Set if these were looked for this time.
        rivals: Option<Vec<String>>,
    },
    /// It's worth looking again at what's open here.
    RefreshApps,
    PeerApps {
        peer: Fingerprint,
        apps: Vec<RunningApp>,
    },
    PeerAppIcon {
        peer: Fingerprint,
        app: String,
        png: Vec<u8>,
    },
    /// A device wants something done with an app here.
    MediaRequested {
        peer: Fingerprint,
        app: String,
        command: MediaCommand,
    },
    /// A media shortcut was pressed on this device's keyboard.
    MediaShortcut(MediaCommand),
    /// A device wants something done with an app's windows here.
    AppActionRequested {
        peer: Fingerprint,
        app: String,
        action: AppAction,
    },
    PairingStage {
        peer_name: Option<String>,
        stage: PairingStage,
    },
    PairingFinished {
        result: Result<Paired, String>,
        responder: bool,
    },
}

struct Session {
    id: u64,
    /// The device that opened this connection.
    initiator: Fingerprint,
    outgoing: UnboundedSender<Message>,
    blobs: BlobChannel,
    /// Set once the peer has introduced itself.
    up: bool,
    /// What the peer said it can do beyond the base protocol.
    features: u32,
    /// Which apps' pictures the peer has been sent on this connection.
    icons_sent: HashSet<String>,
}

/// Which app on which device.
type AppOn = (Fingerprint, String);

/// Files going from here to a folder on another device, or into a folder here.
struct Transfer {
    id: ClipVersion,
    peer: Fingerprint,
    sending: bool,
    label: String,
    total: u64,
    done: u64,
    stage: TransferStage,
    folder: Option<String>,
    /// Set while saving here.
    control: Option<Arc<TransferControl>>,
    /// When there was last any news of it, or the connection for it was last there.
    last_heard: Instant,
}

/// Files a device wants to send here, while the user decides.
struct IncomingFiles {
    id: ClipVersion,
    peer: Fingerprint,
    files: Vec<SharedFile>,
}

fn transfer_key(id: ClipVersion) -> String {
    format!("{}-{}", id.counter, id.owner)
}

/// A pairing in progress or its result, until dismissed.
struct Pairing {
    view: PairingView,
    task: Option<JoinHandle<()>>,
    /// Hands the code the user typed to the waiting task.
    code: Option<oneshot::Sender<String>>,
}

struct Engine<P: Platform> {
    options: Options,
    me: Fingerprint,
    port: u16,
    settings: Settings,
    /// The make and model of this computer.
    model: Option<String>,
    paired: BTreeMap<Fingerprint, PairedDevice>,
    layout: LayoutDoc,
    monitors: Vec<Monitor>,
    topology: Topology,
    discovered: BTreeMap<Fingerprint, FoundDevice>,
    sessions: BTreeMap<Fingerprint, Session>,
    dialing: HashSet<Fingerprint>,
    next_session_id: u64,
    pairing: Option<Pairing>,
    pairing_cooldown_until: Option<Instant>,
    controlled_by: Option<Fingerprint>,
    capture_error: Option<String>,
    clipboard: ClipboardSync,
    /// Devices refused for speaking a different version of the protocol.
    incompatible: HashSet<Fingerprint>,
    /// Connected devices whose users have turned sharing off.
    peers_off: HashSet<Fingerprint>,
    /// What other devices may read from this one.
    offered_files: OfferedFiles,
    /// How to read from other devices.
    links: Links,
    desktop_folder: Option<PathBuf>,
    transfers: Vec<Transfer>,
    /// Waiting for the user, first in line first.
    incoming_files: VecDeque<IncomingFiles>,
    last_transfer_counter: u64,
    trust: TrustStore,
    endpoint: Endpoint,
    discovery: Option<Discovery>,
    shared: Shared<P>,
    events: UnboundedSender<Event>,
    apps: Vec<UnboundedSender<FromAgent>>,
    last_state: Option<UiState>,
    ticks: u64,
    last_tick: Instant,
    /// What's open on this computer. Looking takes a moment and could wait on other
    /// programs, so it's done off to the side, one look at a time.
    local_apps: Vec<RunningApp>,
    reading_apps: bool,
    /// Pictures of the apps here, to send to other devices.
    local_icons: BTreeMap<String, Vec<u8>>,
    icons_looked_for: HashSet<String>,
    /// What's open on other devices, for as long as they're connected.
    peer_apps: BTreeMap<Fingerprint, Vec<RunningApp>>,
    /// Pictures of apps on every device, ready for the app to show.
    icon_urls: BTreeMap<AppOn, String>,
    /// What's playing anywhere, and since when it was noticed.
    playing_since: BTreeMap<AppOn, Instant>,
    /// What was last played, paused or skipped from here. Pausing something leaves nothing
    /// playing, and the same shortcut should start it again.
    last_media_target: Option<AppOn>,
    /// Other programs here that share the keyboard and mouse too.
    rivals: Vec<String>,
    /// Keeps pictures of the screens here from being taken all at once.
    picture_limits: screens::Limits,
    /// Which devices have been asking for pictures of the screens here, and when they last did.
    watchers: BTreeMap<Fingerprint, Instant>,
    // These keep input capture and clipboard watching running.
    capture: Option<Capture>,
    _clipboard_watcher: Option<Capture>,
}

/// Runs the agent until it's asked to stop.
pub async fn run<P: Platform>(options: Options, platform: P) -> Result<()> {
    // First, so a second copy of the agent stops here without touching anything.
    let listener = bind_channel(&options.channel).await.with_context(|| {
        format!(
            "can't open {}: is Hackerman already running?",
            options.channel
        )
    })?;

    let dir = options.config_dir.clone();
    let identity = Identity::load_or_create(&dir.join("identity"))
        .context("can't load or create this computer's identity")?;
    let me = identity.fingerprint();
    let settings = Settings::load(&dir)?;
    let paired: BTreeMap<Fingerprint, PairedDevice> = config::load_devices(&dir)?
        .into_iter()
        .map(|device| (device.fingerprint, device))
        .collect();
    let layout = config::load_layout(&dir)?;

    let trust = TrustStore::new(paired.keys().copied());
    let port = options.port.unwrap_or(settings.port);
    let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let endpoint = bind_endpoint(&identity, listen, &trust)
        .await
        .with_context(|| format!("can't listen on {listen}"))?;
    let port = endpoint.local_addr()?.port();

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let monitors = platform.monitors();
    let model = platform.model();
    let shared_desktop = platform.desktop_folder();
    let shared = Shared::new(
        Switcher::new(me, platform.park_point()),
        platform.clone(),
        events_tx.clone(),
    );

    shared.switcher().set_rules(settings.rules.clone());
    shared.set_accepts_input(settings.enabled);

    let (capture, capture_error) = if options.capture && settings.enabled {
        start_capture(&shared)
    } else {
        (None, None)
    };

    let clipboard_watcher = {
        let events = events_tx.clone();
        let on_change = Box::new(move || {
            let _ = events.send(Event::ClipboardChanged);
        });
        match platform.watch_clipboard(on_change) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(%error, "can't watch the clipboard");
                None
            }
        }
    };

    let discovery = if options.discovery {
        let events = events_tx.clone();
        match Discovery::start(me, &settings.name, model.clone(), port, move |event| {
            let _ = events.send(Event::Discovery(event));
        }) {
            Ok(discovery) => Some(discovery),
            Err(error) => {
                // Pairing by typing an address still works without it.
                tracing::warn!(%error, "can't look for devices on the network");
                None
            }
        }
    } else {
        None
    };

    let listeners = [
        spawn_ipc_listener(listener, events_tx.clone()),
        spawn_network_listener(endpoint.clone(), events_tx.clone()),
    ];

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol = PROTOCOL_VERSION,
        fingerprint = %me,
        port,
        "hackerman agent started"
    );

    let mut engine = Engine {
        options,
        me,
        port,
        settings,
        model,
        paired,
        layout,
        monitors,
        topology: Topology::default(),
        discovered: BTreeMap::new(),
        sessions: BTreeMap::new(),
        dialing: HashSet::new(),
        next_session_id: 0,
        pairing: None,
        pairing_cooldown_until: None,
        controlled_by: None,
        capture_error,
        clipboard: ClipboardSync::new(me),
        incompatible: HashSet::new(),
        peers_off: HashSet::new(),
        offered_files: OfferedFiles::default(),
        links: Links::default(),
        desktop_folder: shared_desktop,
        transfers: Vec::new(),
        incoming_files: VecDeque::new(),
        last_transfer_counter: 0,
        trust,
        endpoint,
        discovery,
        shared,
        events: events_tx,
        apps: Vec::new(),
        last_state: None,
        ticks: 0,
        last_tick: Instant::now(),
        local_apps: Vec::new(),
        reading_apps: false,
        local_icons: BTreeMap::new(),
        icons_looked_for: HashSet::new(),
        peer_apps: BTreeMap::new(),
        icon_urls: BTreeMap::new(),
        playing_since: BTreeMap::new(),
        last_media_target: None,
        rivals: Vec::new(),
        picture_limits: screens::Limits::new(),
        watchers: BTreeMap::new(),
        capture,
        _clipboard_watcher: clipboard_watcher,
    };
    engine.rebuild();
    engine.main_loop(events_rx).await;
    engine.shut_down(listeners).await;
    // Sessions let go of any keys held down on this computer as they end, which they're just
    // about to. Those have to reach Windows before this program stops.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let platform = engine.shared.platform.clone();
    let _ = tokio::task::spawn_blocking(move || platform.finish_injecting()).await;
    tracing::info!("hackerman agent stopped");
    Ok(())
}

/// Starts capturing this computer's input. Returns what keeps it running, or why
/// it couldn't start.
fn start_capture<P: Platform>(shared: &Shared<P>) -> (Option<Capture>, Option<String>) {
    let hook_shared = shared.clone();
    let handler = Box::new(move |event| hook_shared.on_local_input(event));
    match shared.platform.start_capture(handler) {
        Ok(capture) => (Some(capture), None),
        Err(error) => {
            tracing::error!(%error, "can't capture input");
            (None, Some(error.to_string()))
        }
    }
}

/// Starts listening. The lock-screen helper and the normal agent hand this computer's port
/// to each other when the screen locks and unlocks, so the other may be just about to let
/// go of it.
async fn bind_endpoint(
    identity: &Identity,
    listen: SocketAddr,
    trust: &TrustStore,
) -> Result<Endpoint, hk_net::Error> {
    let mut attempts = 0;
    loop {
        match Endpoint::bind(identity, listen, trust.clone()) {
            Err(_) if attempts < 40 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
}

/// Opens the local channel. An agent that is just exiting can briefly still hold
/// it, for example when the app restarts, so this keeps trying for a few seconds.
async fn bind_channel(channel: &str) -> std::io::Result<IpcListener> {
    let mut attempts = 0;
    loop {
        match IpcListener::bind(channel) {
            Ok(listener) => return Ok(listener),
            Err(_) if attempts < 40 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn spawn_ipc_listener(mut listener: IpcListener, events: UnboundedSender<Event>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Owning the connections' tasks means they stop when this task does.
        let mut connections = JoinSet::new();
        loop {
            while connections.try_join_next().is_some() {}
            match listener.accept().await {
                Ok(connection) => {
                    connections.spawn(serve_app(connection, events.clone()));
                }
                Err(error) => {
                    tracing::warn!(%error, "app connection failed");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    })
}

/// Passes requests from one app window to the main loop, and state back.
async fn serve_app(mut connection: IpcConnection, events: UnboundedSender<Event>) {
    let (to_app_tx, mut to_app_rx) = mpsc::unbounded_channel();
    if events.send(Event::AppConnected(to_app_tx.clone())).is_err() {
        return;
    }
    loop {
        tokio::select! {
            message = to_app_rx.recv() => {
                let Some(message) = message else { return };
                if connection.send(&message).await.is_err() {
                    return;
                }
            }
            request = connection.recv::<Request>() => match request {
                Ok(Some(request)) => {
                    // A picture goes back only to the app that asked for it.
                    let event = match request {
                        Request::ScreenPicture { id, ask } => Event::ScreenAsked {
                            id,
                            ask,
                            reply: to_app_tx.clone(),
                        },
                        request => Event::AppRequest(request),
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(%error, "bad request from the app");
                    return;
                }
            },
        }
    }
}

fn spawn_network_listener(endpoint: Endpoint, events: UnboundedSender<Event>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.next_incoming().await {
            let events = events.clone();
            // Each in its own task: a slow or silent device mustn't hold up others.
            tokio::spawn(async move {
                match incoming.establish().await {
                    Ok(accepted) => {
                        let _ = events.send(Event::Incoming(Box::new(accepted)));
                    }
                    // Often harmless: trying several addresses at once means the
                    // losing attempts get dropped as soon as one succeeds.
                    Err(error) => tracing::debug!(%error, "incoming connection didn't complete"),
                }
            });
        }
    })
}

impl<P: Platform> Engine<P> {
    async fn shut_down(&mut self, listeners: [JoinHandle<()>; 2]) {
        // Input first: nothing may be captured by a program that's stopping.
        self.capture = None;
        self._clipboard_watcher = None;
        let actions = self.shared.switcher().peer_took_control();
        self.shared.apply(actions);

        for listener in listeners {
            listener.abort();
            let _ = listener.await;
        }
        if let Some(pairing) = self.pairing.take()
            && let Some(task) = pairing.task
        {
            task.abort();
        }
        self.discovery = None;
        // Whatever is saving files here cleans up after itself, if it's told in time.
        for control in self.transfers.iter().filter_map(|t| t.control.as_ref()) {
            control.cancel();
        }
        // Closing the connections also ends the sessions, which let go of any
        // keys a peer is still holding down on this computer.
        self.endpoint.shutdown().await;
    }

    async fn main_loop(&mut self, mut events: UnboundedReceiver<Event>) {
        let mut tick = tokio::time::interval(TICK);
        let mut fullscreen_check = tokio::time::interval(FULLSCREEN_CHECK);
        fullscreen_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut transfer_check = tokio::time::interval(TRANSFER_CHECK);
        transfer_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut lock_screen_check = tokio::time::interval(LOCK_SCREEN_CHECK);
        lock_screen_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => self.on_tick(),
                _ = lock_screen_check.tick() => {
                    if self.shared.platform.should_step_aside() {
                        tracing::info!("handing this computer over, as the screen has changed");
                        return;
                    }
                }
                _ = fullscreen_check.tick() => self.check_fullscreen(),
                _ = transfer_check.tick() => self.check_transfers(),
                event = events.recv() => {
                    let Some(event) = event else { return };
                    if let Event::AppRequest(Request::Shutdown) = event {
                        return;
                    }
                    self.on_event(event);
                }
            }
        }
    }

    fn on_tick(&mut self) {
        self.ticks += 1;
        let since_last = self.last_tick.elapsed();
        self.last_tick = Instant::now();
        if since_last >= TICK + HELD_UP_AFTER {
            tracing::info!(
                seconds = since_last.as_secs(),
                "the agent was held up, or this computer was asleep"
            );
        }
        self.read_apps();
        self.forget_old_watchers();
        let monitors = self.shared.platform.monitors();
        if monitors != self.monitors {
            tracing::info!(count = monitors.len(), "monitors changed");
            self.monitors = monitors.clone();
            self.broadcast(&Message::Monitors(monitors));
            self.rebuild();
        }
        self.dial_paired_devices();
        // This computer's own IP address may have changed.
        self.notify();
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::AppConnected(app) => {
                let _ = app.send(FromAgent::State(Box::new(self.ui_state())));
                self.apps.push(app);
                // An app that stopped in the middle of choosing one would leave them off.
                self.shared.switcher().set_shortcuts_paused(false);
            }
            Event::AppRequest(request) => self.on_request(request),
            Event::ScreenAsked { id, ask, reply } => self.on_screen_asked(id, ask, reply),
            Event::ScreenRequested {
                peer,
                request,
                writer,
            } => self.on_screen_requested(peer, request, writer),
            Event::Discovery(DiscoveryEvent::Found(found)) => {
                let device = found.fingerprint;
                self.discovered.insert(device, found);
                if self.paired.contains_key(&device) {
                    self.dial_paired_devices();
                }
                self.notify();
            }
            Event::Discovery(DiscoveryEvent::Lost(device)) => {
                self.discovered.remove(&device);
                self.notify();
            }
            Event::Incoming(accepted) => match *accepted {
                Accepted::Link(link) => self.on_link(link, false),
                Accepted::Pairing(channel) => self.on_pairing_request(channel),
            },
            Event::Dialed { device, result } => {
                self.dialing.remove(&device);
                match result {
                    Ok(link) => self.on_link(link, true),
                    Err(error) => tracing::debug!(%device, %error, "couldn't reach paired device"),
                }
            }
            Event::SessionUp {
                id,
                peer,
                name,
                monitors,
                layout,
                model,
                enabled,
                features,
            } => self.on_session_up(id, peer, name, monitors, layout, model, enabled, features),
            Event::SessionDown { id, peer } => self.on_session_down(id, peer),
            Event::PeerMonitors { peer, monitors } => {
                if let Some(device) = self.paired.get_mut(&peer) {
                    device.monitors = sanitize(monitors);
                    self.save_devices();
                    self.rebuild();
                }
            }
            Event::PeerLayout { layout } => self.adopt_layout(layout),
            Event::PeerRenamed { peer, name } => {
                if let Some(device) = self.paired.get_mut(&peer) {
                    device.name = name;
                    self.save_devices();
                    self.notify();
                }
            }
            Event::PeerEnabled { peer, enabled } => self.on_peer_enabled(peer, enabled),
            Event::ControlledBy { peer, active } => {
                if active {
                    // The peer may not have heard yet that this device is turned off.
                    if !self.settings.enabled {
                        return;
                    }
                    // Two people can't both be in charge: the newest input wins.
                    let actions = self.shared.switcher().peer_took_control();
                    self.shared.apply(actions);
                    self.controlled_by = Some(peer);
                    self.fetch_clipboard();
                } else if self.controlled_by == Some(peer) {
                    self.controlled_by = None;
                }
                self.notify();
            }
            Event::PeerIncompatible { peer } => {
                tracing::warn!(%peer, "that device runs a different version of Hackerman");
                self.incompatible.insert(peer);
                self.notify();
            }
            Event::ControlChanged => {
                // Arriving back here is when this device needs the newest clipboard.
                if self.shared.switcher().controlling().is_none() {
                    self.fetch_clipboard();
                }
                self.notify();
            }
            Event::ClipboardChanged => self.on_clipboard_changed(),
            Event::ClipboardOffered { peer, version } => self.on_clipboard_offered(peer, version),
            Event::ClipboardRequested { peer, version } => {
                self.on_clipboard_requested(peer, version);
            }
            Event::ClipboardArrived { peer, bytes } => self.on_clipboard_arrived(peer, bytes),
            Event::ClipboardApplied { version } => self.clipboard.applied(version),
            Event::LocalInput => {
                if self.shared.switcher().controlling().is_none() {
                    self.fetch_clipboard();
                }
            }
            Event::FilesListed { id, files, paths } => self.on_files_listed(id, files, paths),
            Event::FilesOffered { peer, id, files } => self.on_files_offered(peer, id, files),
            Event::FilesSaved { id, result } => self.on_files_saved(id, result),
            Event::TransferUpdated { peer, id, update } => {
                self.on_transfer_updated(peer, id, update);
            }
            Event::TransferCancelled { peer, id } => self.on_transfer_cancelled(peer, id),
            Event::AppsRead {
                apps,
                icons,
                looked_for,
                rivals,
            } => self.on_apps_read(apps, icons, looked_for, rivals),
            Event::RefreshApps => self.read_apps(),
            Event::PeerApps { peer, apps } => self.on_peer_apps(peer, apps),
            Event::PeerAppIcon { peer, app, png } => self.on_peer_app_icon(peer, app, png),
            Event::MediaRequested { peer, app, command } => {
                self.on_media_requested(peer, app, command);
            }
            Event::MediaShortcut(command) => {
                if let Some((device, app)) = self.media_target() {
                    self.send_media_command(device, app, command);
                }
            }
            Event::AppActionRequested { peer, app, action } => {
                self.on_app_action_requested(peer, app, action);
            }
            Event::PairingStage { peer_name, stage } => {
                if let Some(pairing) = &mut self.pairing {
                    pairing.view = PairingView { peer_name, stage };
                    self.notify();
                }
            }
            Event::PairingFinished { result, responder } => {
                self.on_pairing_finished(result, responder);
            }
        }
    }

    fn on_request(&mut self, request: Request) {
        match request {
            Request::Pair { device } => match self.discovered.get(&device) {
                Some(found) => self.start_pairing(found.addresses.clone()),
                None => self.set_pairing_failure("That device is no longer on the network."),
            },
            Request::PairAddress { address } => match parse_address(&address, config::DEFAULT_PORT)
            {
                Some(address) => self.start_pairing(vec![address]),
                None => self.set_pairing_failure("That doesn't look like an IP address."),
            },
            Request::SubmitPairingCode { code } => {
                if let Some(sender) = self
                    .pairing
                    .as_mut()
                    .and_then(|pairing| pairing.code.take())
                {
                    let _ = sender.send(code);
                }
            }
            Request::DismissPairing => {
                if let Some(pairing) = self.pairing.take()
                    && let Some(task) = pairing.task
                {
                    task.abort();
                }
                self.notify();
            }
            Request::Forget { device } => {
                self.trust.remove(device);
                self.paired.remove(&device);
                self.save_devices();
                self.end_session(device);
                self.rebuild();
            }
            Request::SetPlacements { placements } => self.edit_layout(placements),
            Request::Rename { name } => self.rename(name),
            Request::SetRules { rules } => self.set_rules(rules),
            Request::SetLocked { locked } => {
                self.shared.switcher().set_locked(locked);
                self.notify();
            }
            Request::SetEnabled { enabled } => self.set_enabled(enabled),
            Request::SendFiles { device, paths } => self.send_files(device, paths),
            Request::CancelTransfer { id } => self.cancel_transfer(&id),
            Request::DismissTransfer { id } => {
                self.transfers
                    .retain(|t| !t.stage.is_over() || transfer_key(t.id) != id);
                self.notify();
            }
            Request::AnswerIncomingFiles { id, accept, always } => {
                self.answer_incoming_files(&id, accept, always);
            }
            Request::SetFilesAllowed { device, allowed } => {
                self.set_files_allowed(device, allowed);
            }
            Request::MediaCommand {
                device,
                app,
                command,
            } => self.send_media_command(device, app, command),
            Request::RecordingShortcut { active } => {
                self.shared.switcher().set_shortcuts_paused(active);
            }
            Request::AppAction {
                device,
                app,
                action,
            } => self.send_app_action(device, app, action),
            Request::SetScreensAllowed { device, allowed } => {
                self.set_screens_allowed(device, allowed);
            }
            // Handled on the way here, to say where the answer goes.
            Request::ScreenPicture { .. } => {}
            Request::Shutdown => {} // Handled by the main loop.
        }
    }

    /// Turns all sharing on this computer on or off. While it's off the input hooks
    /// aren't even installed, so nothing here can get in the way of the keyboard and
    /// mouse, and the connections stay up to bring everything back at once.
    fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.settings.enabled {
            return;
        }
        self.settings.enabled = enabled;
        if let Err(error) = self.settings.save(&self.options.config_dir) {
            tracing::warn!(%error, "can't save settings");
        }
        tracing::info!(enabled, "sharing turned on or off");
        self.shared.set_accepts_input(enabled);
        if enabled {
            if self.options.capture {
                (self.capture, self.capture_error) = start_capture(&self.shared);
            }
        } else {
            // The hooks first, so that a last movement can't start another crossing.
            // Their window hiding the cursor goes with them, which shows it again.
            self.offered_files.clear();
            self.stop_all_transfers();
            self.capture = None;
            let actions = self.shared.switcher().capture_stopping();
            self.shared.apply(actions);
            self.capture_error = None;
            self.controlled_by = None;
            self.watchers.clear();
        }
        self.check_fullscreen();
        self.broadcast(&Message::Enabled(enabled));
        self.tell_peers_about_apps();
        self.rebuild();
    }

    /// Clipboards don't catch up when a device comes back on. It could have something
    /// newer of its own by then, which is what the user would expect to paste there.
    fn on_peer_enabled(&mut self, peer: Fingerprint, enabled: bool) {
        if enabled {
            self.peers_off.remove(&peer);
        } else {
            self.peers_off.insert(peer);
            self.clipboard.device_gone(peer);
            if self.controlled_by == Some(peer) {
                self.controlled_by = None;
            }
        }
        // Without it, the cursor comes home if it was there.
        self.rebuild();
    }

    fn rename(&mut self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() || name == self.settings.name {
            return;
        }
        self.settings.name = name.clone();
        if let Err(error) = self.settings.save(&self.options.config_dir) {
            tracing::warn!(%error, "can't save settings");
        }
        if let Some(discovery) = &self.discovery
            && let Err(error) = discovery.announce(&name)
        {
            tracing::warn!(%error, "can't update the network announcement");
        }
        self.broadcast(&Message::Renamed(name));
        self.notify();
    }

    fn set_rules(&mut self, rules: Rules) {
        if rules == self.settings.rules {
            return;
        }
        self.settings.rules = rules.clone();
        if let Err(error) = self.settings.save(&self.options.config_dir) {
            tracing::warn!(%error, "can't save settings");
        }
        if !rules.clipboard {
            self.offered_files.clear();
        }
        self.shared.switcher().set_rules(rules);
        self.check_fullscreen();
        self.tell_peers_about_apps();
        self.read_apps();
        self.notify();
    }

    // What's open, and what's playing

    /// Starts a look at what's open here, unless nothing would use the answer. That's the
    /// case for the lock-screen helper, which has no business looking.
    fn read_apps(&mut self) {
        let rules = &self.settings.rules;
        self.apps.retain(|app| !app.is_closed());
        let wanted = rules.share_apps || rules.media_control || !self.apps.is_empty();
        if !wanted || self.reading_apps {
            return;
        }
        self.reading_apps = true;
        let platform = self.shared.platform.clone();
        let events = self.events.clone();
        let mut looked_for = self.icons_looked_for.clone();
        let check_rivals = self.ticks % RIVALS_CHECK_EVERY_N_TICKS == 1;
        tokio::task::spawn_blocking(move || {
            let apps = platform.running_apps();
            let mut icons = Vec::new();
            let mut newly_looked_for = Vec::new();
            for app in &apps {
                if looked_for.insert(app.id.clone()) {
                    newly_looked_for.push(app.id.clone());
                    if let Some(png) = platform.app_icon(&app.id) {
                        icons.push((app.id.clone(), png));
                    }
                }
            }
            let _ = events.send(Event::AppsRead {
                apps,
                icons,
                looked_for: newly_looked_for,
                rivals: check_rivals.then(|| platform.rival_programs()),
            });
        });
    }

    fn on_apps_read(
        &mut self,
        apps: Vec<RunningApp>,
        icons: Vec<(String, Vec<u8>)>,
        looked_for: Vec<String>,
        rivals: Option<Vec<String>>,
    ) {
        self.reading_apps = false;
        self.icons_looked_for.extend(looked_for);
        if let Some(rivals) = rivals
            && rivals != self.rivals
        {
            tracing::warn!(
                ?rivals,
                "other programs sharing the keyboard and mouse are running here"
            );
            self.rivals = rivals;
        }
        let new_icons = !icons.is_empty();
        for (app, png) in icons {
            if png.len() <= MAX_ICON_BYTES {
                self.icon_urls
                    .insert((self.me, app.clone()), icon_url(&png));
                self.local_icons.insert(app, png);
            }
        }
        if apps != self.local_apps || new_icons {
            self.local_apps = apps;
            self.media_changed();
            self.tell_peers_about_apps();
        }
        self.notify();
    }

    /// What other devices get told is open here.
    fn apps_for_peers(&self) -> Vec<RunningApp> {
        let rules = &self.settings.rules;
        if !self.settings.enabled || !rules.share_apps {
            return Vec::new();
        }
        let mut apps = self.local_apps.clone();
        if !rules.media_control {
            for app in &mut apps {
                app.media = None;
            }
        }
        apps
    }

    fn tell_peers_about_apps(&mut self) {
        let peers: Vec<Fingerprint> = self.sessions.keys().copied().collect();
        for peer in peers {
            self.tell_peer_about_apps(peer);
        }
    }

    fn tell_peer_about_apps(&mut self, peer: Fingerprint) {
        let apps = self.apps_for_peers();
        let Some(session) = self
            .sessions
            .get_mut(&peer)
            .filter(|session| session.up && session.features & FEATURE_APPS != 0)
        else {
            return;
        };
        for app in &apps {
            if let Some(png) = self.local_icons.get(&app.id)
                && session.icons_sent.insert(app.id.clone())
            {
                let _ = session.outgoing.send(Message::AppIcon {
                    app: app.id.clone(),
                    png: png.clone(),
                });
            }
        }
        let _ = session.outgoing.send(Message::Apps(apps));
    }

    fn on_peer_apps(&mut self, peer: Fingerprint, apps: Vec<RunningApp>) {
        if !self.sessions.get(&peer).is_some_and(|session| session.up) {
            return;
        }
        self.peer_apps.insert(peer, apps::tidied(apps));
        self.media_changed();
        self.notify();
    }

    fn on_peer_app_icon(&mut self, peer: Fingerprint, app: String, png: Vec<u8>) {
        let has_room = self
            .icon_urls
            .keys()
            .filter(|(device, _)| *device == peer)
            .count()
            < MAX_ICONS;
        // It goes into the page as a picture, so it had better say what it is.
        if !has_room
            || png.len() > MAX_ICON_BYTES
            || !png.starts_with(b"\x89PNG")
            || !self.sessions.get(&peer).is_some_and(|session| session.up)
        {
            return;
        }
        let app = app.chars().take(apps::MAX_APP_TEXT).collect();
        self.icon_urls.insert((peer, app), icon_url(&png));
        self.notify();
    }

    /// Every app on every device that can be played and paused, starting with this device's.
    fn media_apps(&self) -> impl Iterator<Item = (Fingerprint, &RunningApp)> {
        let here = self.local_apps.iter().map(|app| (self.me, app));
        let there = self
            .peer_apps
            .iter()
            .flat_map(|(&device, apps)| apps.iter().map(move |app| (device, app)));
        here.chain(there).filter(|(_, app)| app.media.is_some())
    }

    /// Keeps track of what started playing when, and of whether there's anything for a media
    /// shortcut to do.
    fn media_changed(&mut self) {
        let playing: Vec<AppOn> = self
            .media_apps()
            .filter(|(_, app)| app.media.is_some_and(|media| media.playing == Some(true)))
            .map(|(device, app)| (device, app.id.clone()))
            .collect();
        self.playing_since.retain(|key, _| playing.contains(key));
        for key in playing {
            self.playing_since.entry(key).or_insert_with(Instant::now);
        }
        let available = self.media_apps().next().is_some();
        self.shared.switcher().set_media_available(available);
    }

    /// What a media shortcut is for: whatever started playing last. With nothing playing, what
    /// the last command was for, so that the shortcut that paused something starts it again.
    /// Failing that, a player that doesn't say what it's doing, since it may well be playing.
    fn media_target(&self) -> Option<AppOn> {
        if let Some((key, _)) = self.playing_since.iter().max_by_key(|(_, since)| **since) {
            return Some(key.clone());
        }
        if let Some(last) = &self.last_media_target
            && self
                .media_apps()
                .any(|(device, app)| device == last.0 && app.id == last.1)
        {
            return Some(last.clone());
        }
        self.media_apps()
            .find(|(_, app)| app.media.is_some_and(|media| media.playing.is_none()))
            .or_else(|| self.media_apps().next())
            .map(|(device, app)| (device, app.id.clone()))
    }

    fn send_media_command(&mut self, device: Fingerprint, app: String, command: MediaCommand) {
        // A PC with sharing turned off has nothing to do with the others.
        if device != self.me && !self.settings.enabled {
            return;
        }
        tracing::info!(%device, app, ?command, "a media command");
        self.last_media_target = Some((device, app.clone()));
        if device == self.me {
            self.run_media_command(app, command);
        } else if let Some(session) = self
            .sessions
            .get(&device)
            .filter(|session| session.up && session.features & FEATURE_MEDIA != 0)
        {
            let _ = session.outgoing.send(Message::Media { app, command });
        }
    }

    fn on_media_requested(&mut self, peer: Fingerprint, app: String, command: MediaCommand) {
        let is_player = self
            .local_apps
            .iter()
            .any(|known| known.id == app && known.media.is_some());
        if !self.settings.enabled || !self.settings.rules.media_control || !is_player {
            tracing::info!(%peer, app, "refused a media command from another device");
            return;
        }
        tracing::info!(%peer, app, ?command, "a media command from another device");
        self.run_media_command(app, command);
    }

    /// This waits on the player, so it's done off to the side.
    fn run_media_command(&self, app: String, command: MediaCommand) {
        let platform = self.shared.platform.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = platform.media_command(&app, command) {
                tracing::warn!(%error, app, "a media command didn't work");
            }
            std::thread::sleep(MEDIA_SETTLES);
            let _ = events.send(Event::RefreshApps);
        });
    }

    // Pictures of the screens

    /// The app wants a picture of a screen. One here is only a matter of taking it.
    fn on_screen_asked(&mut self, id: u64, ask: ScreenAsk, reply: UnboundedSender<FromAgent>) {
        let answer_now = |answer: PictureAnswer| {
            let _ = reply.send(FromAgent::Picture(ScreenPicture { id, answer }));
        };
        if ask.device == self.me {
            let Some(monitor) = self.monitors.iter().find(|m| m.id == ask.monitor).cloned() else {
                return answer_now(PictureAnswer::Unavailable);
            };
            let Some(place) = self.picture_limits.join() else {
                return answer_now(PictureAnswer::Busy);
            };
            let platform = self.shared.platform.clone();
            let limits = self.picture_limits.clone();
            tokio::spawn(async move {
                let answer = screens::answer_local(platform, monitor, ask, limits, place).await;
                let _ = reply.send(FromAgent::Picture(ScreenPicture { id, answer }));
            });
            return;
        }
        // A PC with sharing turned off has nothing to do with the others.
        let session = self
            .sessions
            .get(&ask.device)
            .filter(|session| session.up && self.settings.enabled);
        let Some(session) = session.filter(|_| !self.peers_off.contains(&ask.device)) else {
            return answer_now(PictureAnswer::Unavailable);
        };
        if session.features & FEATURE_SCREENS == 0 {
            return answer_now(PictureAnswer::NeedsUpdate);
        }
        let blobs = session.blobs.clone();
        tokio::spawn(async move {
            let answer = screens::ask_peer(blobs, ask).await;
            let _ = reply.send(FromAgent::Picture(ScreenPicture { id, answer }));
        });
    }

    /// Another device wants a picture of a screen here. It only gets one if the user here has
    /// said so for that device, and each one it gets counts as it looking at this PC.
    fn on_screen_requested(
        &mut self,
        peer: Fingerprint,
        request: ScreenRequest,
        writer: DataWriter,
    ) {
        let allowed = self.paired.get(&peer).is_some_and(|d| d.screens_allowed);
        let monitor = self.monitors.iter().find(|m| m.id == request.monitor);
        let refusal = if !allowed {
            Some(ScreenAnswer::NotAllowed)
        } else if !self.settings.enabled || monitor.is_none() {
            Some(ScreenAnswer::Unavailable)
        } else {
            None
        };
        let place = refusal
            .is_none()
            .then(|| self.picture_limits.join())
            .flatten();
        let Some(place) = place else {
            let answer = refusal.unwrap_or(ScreenAnswer::Busy);
            tokio::spawn(async move {
                if let Err(error) = screens::refuse(writer, answer).await {
                    tracing::debug!(%error, "turning down a request for a picture ended early");
                }
            });
            return;
        };
        let Some(monitor) = monitor.cloned() else {
            return;
        };
        if self.watchers.insert(peer, Instant::now()).is_none() {
            tracing::info!(%peer, "another device started looking at this computer's screens");
            self.notify();
        }
        let platform = self.shared.platform.clone();
        let limits = self.picture_limits.clone();
        tokio::spawn(async move {
            let answered =
                screens::answer_peer(platform, monitor, request, writer, limits, place).await;
            if let Err(error) = answered {
                // Usually only that the other side stopped reading.
                tracing::debug!(%error, "sending a picture ended early");
            }
        });
    }

    fn set_screens_allowed(&mut self, device: Fingerprint, allowed: bool) {
        let Some(paired) = self.paired.get_mut(&device) else {
            return;
        };
        paired.screens_allowed = allowed;
        self.save_devices();
        if !allowed {
            self.watchers.remove(&device);
        }
        tracing::info!(%device, allowed, "changed who can see this computer's screens");
        self.notify();
    }

    fn forget_old_watchers(&mut self) {
        let before = self.watchers.len();
        self.watchers.retain(|peer, last| {
            let still = last.elapsed() < WATCHING_FOR;
            if !still {
                tracing::info!(%peer, "another device stopped looking at this computer's screens");
            }
            still
        });
        if self.watchers.len() != before {
            self.notify();
        }
    }

    // Bringing apps up and moving them

    fn send_app_action(&mut self, device: Fingerprint, app: String, action: AppAction) {
        tracing::info!(%device, app, ?action, "doing something with an app's windows");
        if device == self.me {
            self.run_app_action(app, action);
            return;
        }
        // A PC with sharing turned off has nothing to do with the others.
        if !self.settings.enabled {
            return;
        }
        if let Some(session) = self
            .sessions
            .get(&device)
            .filter(|session| session.up && session.features & FEATURE_APP_ACTIONS != 0)
        {
            let _ = session.outgoing.send(Message::AppAction { app, action });
        }
    }

    fn on_app_action_requested(&mut self, peer: Fingerprint, app: String, action: AppAction) {
        let is_open_here = self.local_apps.iter().any(|known| known.id == app);
        if !self.settings.enabled
            || !self.settings.rules.app_control
            || !is_open_here
            || !action.makes_sense()
        {
            tracing::info!(%peer, app, "refused to do something with an app's windows");
            return;
        }
        tracing::info!(%peer, app, ?action, "another device is doing something with an app's windows");
        self.run_app_action(app, action);
    }

    /// This waits on the app, so it's done off to the side. The canvas then gets to hear about
    /// where its windows are now, without waiting for the next look.
    fn run_app_action(&self, app: String, action: AppAction) {
        let platform = self.shared.platform.clone();
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = platform.app_action(&app, &action) {
                tracing::warn!(%error, app, "doing something with an app's windows didn't work");
            }
            std::thread::sleep(MEDIA_SETTLES);
            let _ = events.send(Event::RefreshApps);
        });
    }

    fn apps_on(&self, device: Fingerprint, monitor: &Monitor) -> Vec<AppView> {
        let apps = if device == self.me {
            Some(&self.local_apps)
        } else {
            self.peer_apps.get(&device)
        };
        apps.into_iter()
            .flatten()
            .filter(|app| app.is_on(&monitor.id, monitor.primary))
            .map(|app| AppView {
                id: app.id.clone(),
                name: app.name.clone(),
                icon: self.icon_urls.get(&(device, app.id.clone())).cloned(),
                minimized: app.minimized,
                media: app.media,
                has_window: !app.monitors.is_empty(),
            })
            .collect()
    }

    fn check_fullscreen(&mut self) {
        let fullscreen = self.settings.enabled
            && self.settings.rules.stay_in_fullscreen
            && self.shared.platform.fullscreen_app_in_front();
        let before = self.shared.switcher().held_by_fullscreen();
        self.shared.switcher().set_fullscreen(fullscreen);
        if self.shared.switcher().held_by_fullscreen() != before {
            self.notify();
        }
    }

    // Files sent to be saved in a folder

    fn name_of(&self, device: Fingerprint) -> String {
        self.paired
            .get(&device)
            .map_or_else(|| "that PC".to_string(), |paired| paired.name.clone())
    }

    /// Why files can't go to this device right now, if they can't.
    fn cant_send_because(&self, device: Fingerprint) -> Option<String> {
        if !self.settings.enabled {
            return Some("Hackerman is disabled on this PC".to_string());
        }
        if device == self.me {
            return None;
        }
        let name = self.name_of(device);
        let Some(session) = self.sessions.get(&device).filter(|s| s.up) else {
            return Some(format!("{name} isn't connected"));
        };
        if self.peers_off.contains(&device) {
            return Some(format!("Hackerman is disabled on {name}"));
        }
        if session.features & FEATURE_SEND_FILES == 0 {
            return Some(format!("Update Hackerman on {name} to send files to it"));
        }
        None
    }

    fn next_transfer_id(&mut self) -> ClipVersion {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        self.last_transfer_counter = now.max(self.last_transfer_counter + 1);
        ClipVersion {
            counter: self.last_transfer_counter,
            owner: self.me,
        }
    }

    fn add_transfer(&mut self, transfer: Transfer) {
        self.transfers.push(transfer);
        let finished = self.transfers.iter().filter(|t| t.stage.is_over()).count();
        let mut to_remove = finished.saturating_sub(FINISHED_TRANSFERS_KEPT);
        self.transfers.retain(|t| {
            let remove = to_remove > 0 && t.stage.is_over();
            to_remove -= usize::from(remove);
            !remove
        });
    }

    fn send_files(&mut self, device: Fingerprint, paths: Vec<String>) {
        let roots: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        let names: Vec<&str> = roots
            .iter()
            .filter_map(|root| root.file_name().and_then(|name| name.to_str()))
            .collect();
        let id = self.next_transfer_id();
        let refusal = self.cant_send_because(device);
        self.add_transfer(Transfer {
            id,
            peer: device,
            sending: device != self.me,
            label: files::describe(&names),
            total: 0,
            done: 0,
            stage: match refusal {
                Some(reason) => TransferStage::Failed { reason },
                None => TransferStage::Waiting,
            },
            folder: None,
            control: None,
            last_heard: Instant::now(),
        });
        self.notify();
        if self.transfers.last().is_some_and(|t| t.stage.is_over()) {
            return;
        }
        // Going through a large folder takes a while.
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let (files, paths) = files::list(&roots);
            let _ = events.send(Event::FilesListed { id, files, paths });
        });
    }

    fn on_files_listed(&mut self, id: ClipVersion, files: Vec<SharedFile>, paths: Vec<PathBuf>) {
        let me = self.me;
        let Some(transfer) = self
            .transfers
            .iter_mut()
            .find(|t| t.id == id && t.stage == TransferStage::Waiting)
        else {
            return; // Cancelled meanwhile.
        };
        let peer = transfer.peer;
        transfer.total = files.iter().map(|file| file.size).sum();
        if files.is_empty() {
            transfer.stage = TransferStage::Failed {
                reason: "Those files can't be read".to_string(),
            };
            self.notify();
            return;
        }
        if peer == me {
            self.start_saving(me, id, files, Arc::new(LocalFiles(paths)));
            return;
        }
        self.offered_files.offer_transfer(id, paths);
        self.notify();

        let Some(session) = self.sessions.get(&peer).filter(|s| s.up) else {
            return; // Noticed by check_transfers if it doesn't come back.
        };
        let blobs = session.blobs.clone();
        let blob = ClipboardBlob {
            version: id,
            data: ClipboardData::default(),
            files,
            save_to_folder: true,
        };
        let events = self.events.clone();
        tokio::spawn(async move {
            let sent = match hk_proto::encode_clipboard(&blob) {
                Ok(bytes) if bytes.len() <= MAX_BLOB_LEN => blobs
                    .send(&bytes)
                    .await
                    .map_err(|error| format!("sending the list of files failed: {error}")),
                Ok(_) => Err("that's too many files to send at once".to_string()),
                Err(error) => Err(error.to_string()),
            };
            if let Err(reason) = sent {
                let update = TransferUpdate::Failed(reason);
                let _ = events.send(Event::TransferUpdated { peer, id, update });
            }
        });
    }

    fn tell_sender(&self, peer: Fingerprint, id: ClipVersion, update: TransferUpdate) {
        if let Some(session) = self.sessions.get(&peer).filter(|s| s.up) {
            let _ = session.outgoing.send(Message::Transfer(id, update));
        }
    }

    fn on_files_offered(&mut self, peer: Fingerprint, id: ClipVersion, files: Vec<SharedFile>) {
        let refuse = |engine: &Self, reason: String| {
            tracing::info!(%peer, reason, "refused files sent to this PC");
            engine.tell_sender(peer, id, TransferUpdate::Failed(reason));
        };
        let here = self.settings.name.clone();
        if id.owner != peer {
            tracing::warn!(%peer, "refused files offered on behalf of another device");
            return;
        }
        if !self.settings.enabled {
            return refuse(self, format!("Hackerman is disabled on {here}"));
        }
        if files.is_empty()
            || files.len() > MAX_SHARED_FILES
            || !files.iter().all(SharedFile::has_safe_path)
        {
            return refuse(self, "Some of those file names aren't safe".to_string());
        }
        let allowed = self
            .paired
            .get(&peer)
            .is_some_and(|device| device.files_allowed);
        match self.settings.rules.receive_files {
            ReceiveFiles::Never => refuse(self, format!("{here} doesn't accept files")),
            ReceiveFiles::AnyDevice => self.start_receiving(peer, id, files),
            ReceiveFiles::AskFirst if allowed => self.start_receiving(peer, id, files),
            ReceiveFiles::AskFirst | ReceiveFiles::Ask => {
                self.incoming_files
                    .push_back(IncomingFiles { id, peer, files });
                self.notify();
            }
        }
    }

    fn start_receiving(&mut self, peer: Fingerprint, id: ClipVersion, files: Vec<SharedFile>) {
        let control = Arc::new(TransferControl::default());
        let source = RemoteFiles {
            runtime: Handle::current(),
            links: self.links.clone(),
            accepts_input: self.shared.accepts_input(),
            control: Some(control.clone()),
            version: id,
            sizes: files.iter().map(|file| file.size).collect(),
        };
        self.start_saving_with(peer, id, files, Arc::new(source), control);
    }

    fn start_saving(
        &mut self,
        peer: Fingerprint,
        id: ClipVersion,
        files: Vec<SharedFile>,
        source: Arc<dyn FileSource>,
    ) {
        self.start_saving_with(peer, id, files, source, Arc::default());
    }

    fn start_saving_with(
        &mut self,
        peer: Fingerprint,
        id: ClipVersion,
        files: Vec<SharedFile>,
        source: Arc<dyn FileSource>,
        control: Arc<TransferControl>,
    ) {
        let chosen = self.settings.rules.drop_folder.as_deref();
        let folder = chosen
            .filter(|folder| !folder.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| self.shared.platform.desktop_folder());
        let label = files::describe(&files::top_level_names(&files));
        let total = files.iter().map(|file| file.size).sum();
        let stage = if folder.is_some() {
            TransferStage::Running
        } else {
            TransferStage::Failed {
                reason: "There's no folder to save them in".to_string(),
            }
        };
        let view_folder = folder.as_ref().map(|folder| folder.display().to_string());

        // Files saved here from here already have an entry, from when they were sent.
        match self.transfers.iter_mut().find(|t| t.id == id) {
            Some(transfer) => {
                transfer.total = total;
                transfer.stage = stage.clone();
                transfer.folder.clone_from(&view_folder);
                transfer.control = Some(control.clone());
            }
            None => self.add_transfer(Transfer {
                id,
                peer,
                sending: false,
                label,
                total,
                done: 0,
                stage: stage.clone(),
                folder: view_folder,
                control: Some(control.clone()),
                last_heard: Instant::now(),
            }),
        }
        self.notify();

        let Some(folder) = folder else {
            let reason = "There's no folder to save them in".to_string();
            self.tell_sender(peer, id, TransferUpdate::Failed(reason));
            return;
        };
        self.tell_sender(peer, id, TransferUpdate::Accepted);
        tracing::info!(%peer, files = files.len(), total, folder = %folder.display(), "saving files");

        let clash = self.settings.rules.name_clash;
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let result = files::save_all(&files, source.as_ref(), &folder, clash, &control);
            let _ = events.send(Event::FilesSaved { id, result });
        });
    }

    fn on_files_saved(&mut self, id: ClipVersion, result: Result<(), SaveError>) {
        let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let peer = transfer.peer;
        let update = match result {
            Ok(()) => {
                transfer.done = transfer.total;
                transfer.stage = TransferStage::Done;
                TransferUpdate::Saved {
                    folder: transfer.folder.clone().unwrap_or_default(),
                }
            }
            Err(SaveError::Cancelled) => {
                transfer.stage = TransferStage::Cancelled;
                TransferUpdate::Cancelled
            }
            Err(SaveError::Failed(reason)) => {
                tracing::warn!(reason, "saving files failed");
                transfer.stage = TransferStage::Failed {
                    reason: reason.clone(),
                };
                TransferUpdate::Failed(reason)
            }
        };
        self.tell_sender(peer, id, update);
        self.notify();
    }

    fn on_transfer_updated(&mut self, peer: Fingerprint, id: ClipVersion, update: TransferUpdate) {
        let Some(transfer) = self
            .transfers
            .iter_mut()
            .find(|t| t.id == id && t.sending && t.peer == peer && !t.stage.is_over())
        else {
            return;
        };
        transfer.last_heard = Instant::now();
        let mut over = true;
        match update {
            TransferUpdate::Accepted => {
                transfer.stage = TransferStage::Running;
                over = false;
            }
            TransferUpdate::Progress(done) => {
                transfer.stage = TransferStage::Running;
                transfer.done = done.min(transfer.total);
                over = false;
            }
            TransferUpdate::Saved { folder } => {
                transfer.stage = TransferStage::Done;
                transfer.done = transfer.total;
                transfer.folder = Some(folder.chars().take(MAX_REPORTED_TEXT).collect());
            }
            TransferUpdate::Failed(reason) => {
                transfer.stage = TransferStage::Failed {
                    reason: reason.chars().take(MAX_REPORTED_TEXT).collect(),
                };
            }
            TransferUpdate::Cancelled => transfer.stage = TransferStage::Cancelled,
        }
        if over {
            self.offered_files.withdraw(id);
        }
        self.notify();
    }

    fn on_transfer_cancelled(&mut self, peer: Fingerprint, id: ClipVersion) {
        self.incoming_files
            .retain(|incoming| incoming.id != id || incoming.peer != peer);
        let saving = self.transfers.iter().find(|t| t.id == id && t.peer == peer);
        if let Some(control) = saving.and_then(|t| t.control.as_ref()) {
            control.cancel();
        }
        self.notify();
    }

    fn cancel_transfer(&mut self, id: &str) {
        let Some(transfer) = self
            .transfers
            .iter_mut()
            .find(|t| transfer_key(t.id) == id && !t.stage.is_over())
        else {
            return;
        };
        // What's saving it reports back that it's stopped.
        if let Some(control) = &transfer.control {
            control.cancel();
            return;
        }
        transfer.stage = TransferStage::Cancelled;
        let (id, peer) = (transfer.id, transfer.peer);
        self.offered_files.withdraw(id);
        if let Some(session) = self.sessions.get(&peer).filter(|s| s.up) {
            let _ = session.outgoing.send(Message::TransferCancel(id));
        }
        self.notify();
    }

    fn answer_incoming_files(&mut self, id: &str, accept: bool, always: bool) {
        let position = self
            .incoming_files
            .iter()
            .position(|incoming| transfer_key(incoming.id) == id);
        let Some(incoming) = position.and_then(|at| self.incoming_files.remove(at)) else {
            return;
        };
        let peer = incoming.peer;
        if accept {
            self.start_receiving(peer, incoming.id, incoming.files);
            if always {
                self.set_files_allowed(peer, true);
            }
        } else {
            let reason = format!("{} turned those files down", self.settings.name);
            self.tell_sender(peer, incoming.id, TransferUpdate::Failed(reason));
        }
        self.notify();
    }

    /// Whether a device can send files here without asking. Once it can, anything else it
    /// has waiting for an answer goes ahead, since the answer would be the same.
    fn set_files_allowed(&mut self, device: Fingerprint, allowed: bool) {
        let Some(paired) = self.paired.get_mut(&device) else {
            return;
        };
        paired.files_allowed = allowed;
        self.save_devices();
        if allowed {
            let (waiting, others): (VecDeque<_>, VecDeque<_>) =
                std::mem::take(&mut self.incoming_files)
                    .into_iter()
                    .partition(|incoming| incoming.peer == device);
            self.incoming_files = others;
            for incoming in waiting {
                self.start_receiving(device, incoming.id, incoming.files);
            }
        }
        self.notify();
    }

    /// Disabling stops everything at once.
    fn stop_all_transfers(&mut self) {
        let reason = format!("Hackerman is disabled on {}", self.settings.name);
        for incoming in std::mem::take(&mut self.incoming_files) {
            self.tell_sender(
                incoming.peer,
                incoming.id,
                TransferUpdate::Failed(reason.clone()),
            );
        }
        for transfer in self.transfers.iter_mut().filter(|t| !t.stage.is_over()) {
            match &transfer.control {
                Some(control) => control.cancel(),
                None => {
                    transfer.stage = TransferStage::Cancelled;
                    if let Some(session) = self.sessions.get(&transfer.peer).filter(|s| s.up) {
                        let _ = session.outgoing.send(Message::TransferCancel(transfer.id));
                    }
                }
            }
        }
    }

    /// Keeps the other end and the app up to date with how much is done, and notices a
    /// transfer whose other end has gone away for good.
    fn check_transfers(&mut self) {
        let mut changed = false;
        let mut lost = Vec::new();
        for transfer in self.transfers.iter_mut().filter(|t| !t.stage.is_over()) {
            if let Some(control) = &transfer.control {
                let done = control.done();
                if done != transfer.done {
                    transfer.done = done;
                    changed = true;
                    if let Some(session) = self.sessions.get(&transfer.peer).filter(|s| s.up) {
                        let update = TransferUpdate::Progress(done);
                        let _ = session
                            .outgoing
                            .send(Message::Transfer(transfer.id, update));
                    }
                }
            } else if transfer.sending {
                if self.sessions.get(&transfer.peer).is_some_and(|s| s.up) {
                    transfer.last_heard = Instant::now();
                } else if transfer.last_heard.elapsed() >= TRANSFER_LOST_AFTER {
                    lost.push((transfer.id, transfer.peer));
                }
            }
        }
        for (id, peer) in lost {
            let reason = format!("Lost contact with {}", self.name_of(peer));
            self.on_transfer_updated(peer, id, TransferUpdate::Failed(reason));
        }
        if changed {
            self.notify();
        }
    }

    // Clipboard

    fn sharing_clipboard(&self) -> bool {
        self.settings.enabled && self.settings.rules.clipboard
    }

    /// Whether the person using these computers is using this one now. A clipboard
    /// that changes on a computer nobody's using is some program doing it, and it
    /// shouldn't replace what the user just copied somewhere else.
    fn in_use(&self) -> bool {
        // Without input capture there's no telling, so assume it is. Crossing away
        // right after copying still counts, because Windows reports the copy a
        // moment later, and the cursor may have left by then.
        self.controlled_by.is_some() || self.capture.is_none() || self.shared.in_use_locally()
    }

    fn on_clipboard_changed(&mut self) {
        if !self.settings.enabled {
            self.clipboard.changed_privately();
            return;
        }
        if !self.settings.rules.clipboard {
            return;
        }
        if !self.in_use() {
            tracing::info!("ignored a clipboard change on a PC nobody is using");
            return;
        }
        let version = self.clipboard.copied();
        tracing::info!(connected = self.sessions.len(), "something was copied here");
        self.broadcast(&Message::ClipboardOffer(version));
    }

    fn on_clipboard_offered(&mut self, peer: Fingerprint, version: ClipVersion) {
        if !self.sharing_clipboard() || version.owner != peer {
            return;
        }
        if !self.clipboard.offered(version) {
            return;
        }
        if self.controlled_by.is_some() {
            self.fetch_clipboard();
        } else {
            // It's fetched when this device is used next, which may be never.
            self.shared.report_next_local_input();
        }
    }

    /// Asks for the newest clipboard, if this device doesn't have it yet.
    fn fetch_clipboard(&mut self) {
        if !self.sharing_clipboard() {
            return;
        }
        let Some(version) = self.clipboard.next_to_fetch() else {
            return;
        };
        let owner_is_on = !self.peers_off.contains(&version.owner);
        match self
            .sessions
            .get(&version.owner)
            .filter(|s| s.up && owner_is_on)
        {
            Some(session) => {
                tracing::info!(owner = %version.owner, "asking for the clipboard");
                let _ = session.outgoing.send(Message::ClipboardRequest(version));
            }
            None => {
                // Pairing is what connects two devices, and the clipboard goes directly.
                tracing::info!(owner = %version.owner, "the newest clipboard is on a PC that isn't connected to this one");
                self.clipboard.device_gone(version.owner);
            }
        }
    }

    fn on_clipboard_requested(&mut self, peer: Fingerprint, version: ClipVersion) {
        if !self.sharing_clipboard() || !self.clipboard.owns(version) {
            tracing::info!(%peer, "asked for a clipboard that has changed since, or isn't shared");
            return;
        }
        let Some(session) = self.sessions.get(&peer).filter(|s| s.up) else {
            return;
        };
        let blobs = session.blobs.clone();
        let takes_files = session.features & FEATURE_FILES != 0;
        let platform = self.shared.platform.clone();
        let offered = self.offered_files.clone();
        tokio::spawn(async move {
            // Reading the clipboard can wait on other programs, and encoding an
            // image or going through a large folder takes a while, so none of that
            // may hold up the async threads.
            let encoded = tokio::task::spawn_blocking(move || {
                let data = platform.read_clipboard()?;
                let mut files = Vec::new();
                if data.is_empty() {
                    let copied = platform.clipboard_files()?;
                    if !copied.is_empty() && !takes_files {
                        tracing::info!(%peer, "can't send files until that PC is updated");
                        return Ok(None);
                    }
                    let (listed, paths) = files::list(&copied);
                    offered.offer(version, paths);
                    files = listed;
                }
                if data.is_empty() && files.is_empty() {
                    tracing::info!(%peer, "the clipboard holds nothing that can be shared");
                    return Ok(None);
                }
                let count = files.len();
                let blob = ClipboardBlob {
                    version,
                    data,
                    files,
                    save_to_folder: false,
                };
                anyhow::Ok(Some((hk_proto::encode_clipboard(&blob)?, count)))
            })
            .await;
            match encoded {
                Ok(Ok(Some((bytes, files)))) if bytes.len() <= MAX_BLOB_LEN => {
                    tracing::info!(%peer, size = bytes.len(), files, "sending the clipboard");
                    if let Err(error) = blobs.send(&bytes).await {
                        tracing::warn!(%error, "sending the clipboard failed");
                    }
                }
                Ok(Ok(Some((bytes, _)))) => {
                    tracing::warn!(size = bytes.len(), "the clipboard is too large to share");
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => tracing::warn!("reading the clipboard failed: {error:#}"),
                Err(error) => tracing::warn!(%error, "reading the clipboard failed"),
            }
        });
    }

    fn on_clipboard_arrived(&mut self, peer: Fingerprint, bytes: Vec<u8>) {
        let sharing_clipboard = self.sharing_clipboard();
        let platform = self.shared.platform.clone();
        let events = self.events.clone();
        let wanted = self.clipboard.next_wanted(peer);
        let runtime = Handle::current();
        let links = self.links.clone();
        let accepts_input = self.shared.accepts_input();
        tokio::task::spawn_blocking(move || {
            let blob = match hk_proto::decode_clipboard(&bytes) {
                Ok(blob) => blob,
                Err(error) => {
                    tracing::warn!(%error, "got an unreadable clipboard");
                    return;
                }
            };
            if blob.save_to_folder {
                let _ = events.send(Event::FilesOffered {
                    peer,
                    id: blob.version,
                    files: blob.files,
                });
                return;
            }
            if !sharing_clipboard {
                return;
            }
            if blob.version.owner != peer || wanted != Some(blob.version) {
                tracing::info!(%peer, "a clipboard arrived too late: something newer was copied");
                return;
            }
            let written = if blob.files.is_empty() {
                platform.write_clipboard(&blob.data)
            } else {
                // These names are about to reach File Explorer, from another computer.
                if blob.files.len() > MAX_SHARED_FILES
                    || !blob.files.iter().all(SharedFile::has_safe_path)
                {
                    tracing::warn!(%peer, "refused a list of files with unsafe names");
                    return;
                }
                let source = RemoteFiles {
                    runtime,
                    links,
                    accepts_input,
                    control: None,
                    version: blob.version,
                    sizes: blob.files.iter().map(|file| file.size).collect(),
                };
                platform.write_clipboard_files(blob.files, Arc::new(source))
            };
            match written {
                Ok(()) => {
                    tracing::info!(%peer, "the clipboard arrived");
                    let _ = events.send(Event::ClipboardApplied {
                        version: blob.version,
                    });
                }
                Err(error) => tracing::warn!(%error, "writing the clipboard failed"),
            }
        });
    }

    // Connections

    fn dial_paired_devices(&mut self) {
        let me = self.me;
        let ticks = self.ticks;
        let targets: Vec<(Fingerprint, Vec<SocketAddr>)> = self
            .paired
            .values()
            .filter(|device| {
                !self.sessions.contains_key(&device.fingerprint)
                    && !self.dialing.contains(&device.fingerprint)
                    && (me < device.fingerprint || ticks.is_multiple_of(HIGHER_DIALS_EVERY_N_TICKS))
                    && (!self.incompatible.contains(&device.fingerprint)
                        || ticks.is_multiple_of(INCOMPATIBLE_DIALS_EVERY_N_TICKS))
            })
            .map(|device| (device.fingerprint, self.known_addresses(device)))
            .filter(|(_, addresses)| !addresses.is_empty())
            .collect();

        for (device, addresses) in targets {
            self.dialing.insert(device);
            let endpoint = self.endpoint.clone();
            let events = self.events.clone();
            tokio::spawn(async move {
                let result = endpoint.connect_any(&addresses, device).await;
                let _ = events.send(Event::Dialed { device, result });
            });
        }
    }

    /// Fresh addresses from the network, then where it was last reached.
    fn known_addresses(&self, device: &PairedDevice) -> Vec<SocketAddr> {
        let mut addresses: Vec<SocketAddr> = self
            .discovered
            .get(&device.fingerprint)
            .map(|found| found.addresses.clone())
            .unwrap_or_default();
        for address in &device.addresses {
            if !addresses.contains(address) {
                addresses.push(*address);
            }
        }
        addresses
    }

    fn on_link(&mut self, link: Link, dialed_by_me: bool) {
        let peer = link.peer();
        let initiator = if dialed_by_me { self.me } else { peer };
        // If both dialed at once, both must keep the same connection: the one
        // opened by the lower fingerprint.
        let mut already_up = false;
        if let Some(existing) = self.sessions.get(&peer) {
            let preferred = self.me.min(peer);
            if existing.initiator == preferred || initiator != preferred {
                tracing::debug!(%peer, "dropping a duplicate connection");
                return;
            }
            // The peer stays reachable through the old connection until the new
            // one is ready, when its sender is replaced and the old session ends.
            // Otherwise input sent in between would be lost.
            already_up = existing.up;
            self.sessions.remove(&peer);
        }

        let id = self.next_session_id;
        self.next_session_id += 1;
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let _ = outgoing_tx.send(self.hello());

        tracing::debug!(%peer, address = %link.remote_address(), dialed_by_me, "connection opened");
        if let Some(device) = self.paired.get_mut(&peer) {
            let address = link.remote_address();
            if device.addresses != [address] {
                device.addresses = vec![address];
                self.save_devices();
            }
        }
        self.sessions.insert(
            peer,
            Session {
                id,
                initiator,
                outgoing: outgoing_tx,
                blobs: link.blobs(),
                up: already_up,
                features: 0,
                icons_sent: HashSet::new(),
            },
        );
        tokio::spawn(session::run(
            link,
            id,
            self.shared.platform.clone(),
            self.shared.accepts_input(),
            self.offered_files.clone(),
            self.events.clone(),
            outgoing_rx,
        ));
    }

    fn hello(&self) -> Message {
        Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: self.settings.name.clone(),
            monitors: self.monitors.clone(),
            layout: self.layout.clone(),
            model: self.model.clone(),
            enabled: self.settings.enabled,
            features: FEATURES,
        }
    }

    #[allow(clippy::too_many_arguments)] // Everything in the peer's introduction.
    fn on_session_up(
        &mut self,
        id: u64,
        peer: Fingerprint,
        name: String,
        monitors: Vec<Monitor>,
        layout: LayoutDoc,
        model: Option<String>,
        enabled: bool,
        features: u32,
    ) {
        let sharing_clipboard = self.sharing_clipboard();
        let Some(session) = self.sessions.get_mut(&peer).filter(|s| s.id == id) else {
            return;
        };
        session.up = true;
        session.features = features;
        self.shared.add_peer(peer, session.outgoing.clone());
        self.links
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer, session.blobs.clone());
        tracing::info!(%peer, %name, "connected");
        self.incompatible.remove(&peer);
        if enabled {
            self.peers_off.remove(&peer);
        } else {
            self.peers_off.insert(peer);
        }
        if let Some(version) = self.clipboard.own_offer()
            && sharing_clipboard
        {
            let _ = session.outgoing.send(Message::ClipboardOffer(version));
        }

        if let Some(device) = self.paired.get_mut(&peer) {
            device.name = name;
            device.model = model.map(|model| model.chars().take(MAX_MODEL_LENGTH).collect());
            device.monitors = sanitize(monitors);
            self.save_devices();
        }
        // Each side sent its copy when the connection opened, so normally both
        // now end up with the newer one. This side's copy may have changed since.
        if self.layout.is_newer_than(&layout)
            && let Some(session) = self.sessions.get(&peer)
        {
            let _ = session.outgoing.send(Message::Layout(self.layout.clone()));
        }
        self.adopt_layout(layout);
        self.tell_peer_about_apps(peer);
        self.rebuild();
    }

    fn on_session_down(&mut self, id: u64, peer: Fingerprint) {
        if self.sessions.get(&peer).is_some_and(|s| s.id == id) {
            tracing::info!(%peer, "disconnected");
            self.end_session(peer);
            self.rebuild();
        }
    }

    /// Closes the connection to `device`, if there is one. Dropping every sender
    /// for its queue ends the session task, which tidies up after itself.
    fn end_session(&mut self, device: Fingerprint) {
        self.sessions.remove(&device);
        self.peers_off.remove(&device);
        self.clipboard.device_gone(device);
        self.shared.remove_peer(device);
        self.links
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&device);
        if self.controlled_by == Some(device) {
            self.controlled_by = None;
        }
        self.peer_apps.remove(&device);
        self.icon_urls.retain(|(owner, _), _| *owner != device);
        self.media_changed();
        self.watchers.remove(&device);
    }

    fn broadcast(&self, message: &Message) {
        for session in self.sessions.values().filter(|session| session.up) {
            let _ = session.outgoing.send(message.clone());
        }
    }

    // Layout

    fn device_monitors(&self) -> Vec<DeviceMonitors> {
        let mut devices = vec![DeviceMonitors {
            device: self.me,
            monitors: self.monitors.clone(),
            online: true,
        }];
        devices.extend(self.paired.values().map(|device| {
            DeviceMonitors {
                device: device.fingerprint,
                monitors: device.monitors.clone(),
                online: self
                    .sessions
                    .get(&device.fingerprint)
                    .is_some_and(|session| session.up)
                    && !self.peers_off.contains(&device.fingerprint),
            }
        }));
        devices
    }

    /// Recalculates the canvas after anything it depends on has changed.
    fn rebuild(&mut self) {
        self.topology = Topology::build(&self.device_monitors(), &self.layout);
        let jump_order = std::iter::once(self.me)
            .chain(self.paired.keys().copied())
            .collect();
        self.shared.switcher().set_jump_order(jump_order);
        let park = self.shared.platform.park_point();
        let actions = self
            .shared
            .switcher()
            .set_environment(self.topology.clone(), park);
        self.shared.apply(actions);
        self.notify();
    }

    fn edit_layout(&mut self, placements: Vec<Placement>) {
        let devices = self.device_monitors();
        self.layout.apply_edit(self.me, &devices, placements);
        self.save_layout();
        self.broadcast(&Message::Layout(self.layout.clone()));
        self.rebuild();
    }

    fn adopt_layout(&mut self, layout: LayoutDoc) {
        if layout.is_newer_than(&self.layout) {
            self.layout.adopt(layout);
            self.save_layout();
            self.rebuild();
        }
    }

    fn save_layout(&self) {
        if let Err(error) = config::save_layout(&self.options.config_dir, &self.layout) {
            tracing::warn!(%error, "can't save the layout");
        }
    }

    fn save_devices(&self) {
        let devices: Vec<PairedDevice> = self.paired.values().cloned().collect();
        if let Err(error) = config::save_devices(&self.options.config_dir, &devices) {
            tracing::warn!(%error, "can't save the paired devices");
        }
    }

    // Pairing

    fn pairing_in_progress(&self) -> bool {
        self.pairing.as_ref().is_some_and(|pairing| {
            !matches!(
                pairing.view.stage,
                PairingStage::Paired | PairingStage::Failed { .. }
            )
        })
    }

    fn start_pairing(&mut self, addresses: Vec<SocketAddr>) {
        if self.pairing_in_progress() {
            return;
        }
        let (code_tx, code_rx) = oneshot::channel();
        let task = tokio::spawn(pairing::initiate(
            self.endpoint.clone(),
            addresses,
            self.settings.name.clone(),
            code_rx,
            self.events.clone(),
        ));
        self.pairing = Some(Pairing {
            view: PairingView {
                peer_name: None,
                stage: PairingStage::Connecting,
            },
            task: Some(task),
            code: Some(code_tx),
        });
        self.notify();
    }

    fn on_pairing_request(&mut self, channel: PairingChannel) {
        let cooling_down = self
            .pairing_cooldown_until
            .is_some_and(|until| Instant::now() < until);
        self.apps.retain(|app| !app.is_closed());
        let has_screen = !self.apps.is_empty() || self.options.headless_pairing;
        if self.pairing_in_progress() || cooling_down || !has_screen {
            tracing::info!(
                peer = %channel.peer(),
                busy = self.pairing_in_progress(),
                cooling_down,
                has_screen,
                "refused a pairing request"
            );
            return; // Dropping the channel hangs up.
        }
        let task = tokio::spawn(pairing::respond(
            channel,
            self.settings.name.clone(),
            generate_code(),
            self.options.headless_pairing,
            self.events.clone(),
        ));
        self.pairing = Some(Pairing {
            view: PairingView {
                peer_name: None,
                stage: PairingStage::Connecting,
            },
            task: Some(task),
            code: None,
        });
        self.notify();
    }

    fn set_pairing_failure(&mut self, reason: &str) {
        self.pairing = Some(Pairing {
            view: PairingView {
                peer_name: None,
                stage: PairingStage::Failed {
                    reason: reason.to_string(),
                },
            },
            task: None,
            code: None,
        });
        self.notify();
    }

    fn on_pairing_finished(&mut self, result: Result<Paired, String>, responder: bool) {
        let Some(pairing) = &mut self.pairing else {
            return; // Dismissed while finishing.
        };
        pairing.task = None;
        pairing.code = None;
        match result {
            Ok(paired) => {
                pairing.view = PairingView {
                    peer_name: Some(paired.name.clone()),
                    stage: PairingStage::Paired,
                };
                tracing::info!(device = %paired.fingerprint, name = %paired.name, "paired");
                self.trust.add(paired.fingerprint);
                self.paired.insert(
                    paired.fingerprint,
                    PairedDevice {
                        fingerprint: paired.fingerprint,
                        name: paired.name,
                        addresses: vec![paired.address],
                        model: None,
                        monitors: Vec::new(),
                        files_allowed: false,
                        screens_allowed: false,
                    },
                );
                self.save_devices();
                self.dial_paired_devices();
                self.rebuild();
            }
            Err(reason) => {
                pairing.view.stage = PairingStage::Failed { reason };
                if responder {
                    self.pairing_cooldown_until = Some(Instant::now() + PAIRING_COOLDOWN);
                }
                self.notify();
            }
        }
    }

    // What the app shows

    fn notify(&mut self) {
        self.apps.retain(|app| !app.is_closed());
        if self.apps.is_empty() {
            self.last_state = None;
            return;
        }
        let state = self.ui_state();
        if self.last_state.as_ref() == Some(&state) {
            return;
        }
        for app in &self.apps {
            let _ = app.send(FromAgent::State(Box::new(state.clone())));
        }
        self.last_state = Some(state);
    }

    fn ui_state(&self) -> UiState {
        // Read together under one lock. A lock taken inside the struct expression
        // below would stay held until its end, and taking it twice there would
        // deadlock with itself, and freeze input, which waits on this lock.
        let (controlling, locked, held_by_fullscreen) = {
            let switcher = self.shared.switcher();
            (
                switcher.controlling(),
                switcher.locked(),
                switcher.held_by_fullscreen(),
            )
        };
        let control = match (controlling, self.controlled_by) {
            (Some(device), _) => ControlView::Controlling { device },
            (None, Some(device)) => ControlView::ControlledBy { device },
            (None, None) => ControlView::Home,
        };
        UiState {
            me: ThisDevice {
                fingerprint: self.me,
                name: self.settings.name.clone(),
                model: self.model.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                addresses: local_addresses().iter().map(ToString::to_string).collect(),
                port: self.port,
            },
            devices: self.device_views(),
            screens: self
                .topology
                .screens
                .iter()
                .map(|screen| ScreenView {
                    device: screen.device,
                    monitor: screen.monitor.id.clone(),
                    name: screen.monitor.name.clone(),
                    x: screen.canvas.x,
                    y: screen.canvas.y,
                    width: screen.canvas.width,
                    height: screen.canvas.height,
                    custom_width: screen.custom_width,
                    pixel_width: screen.monitor.bounds.width,
                    pixel_height: screen.monitor.bounds.height,
                    scale: screen.monitor.scale,
                    primary: screen.monitor.primary,
                    online: screen.online,
                    apps: self.apps_on(screen.device, &screen.monitor),
                })
                .collect(),
            pairing: self.pairing.as_ref().map(|pairing| pairing.view.clone()),
            control,
            rules: self.settings.rules.clone(),
            enabled: self.settings.enabled,
            locked,
            held_by_fullscreen,
            capture_error: self.capture_error.clone(),
            desktop_folder: self
                .desktop_folder
                .as_ref()
                .map(|folder| folder.display().to_string()),
            transfers: self
                .transfers
                .iter()
                .map(|transfer| TransferView {
                    id: transfer_key(transfer.id),
                    peer: transfer.peer,
                    sending: transfer.sending,
                    label: transfer.label.clone(),
                    total_bytes: transfer.total,
                    done_bytes: transfer.done,
                    stage: transfer.stage.clone(),
                    folder: transfer.folder.clone(),
                })
                .collect(),
            incoming_files: self
                .incoming_files
                .front()
                .map(|incoming| IncomingFilesView {
                    id: transfer_key(incoming.id),
                    peer_name: self.name_of(incoming.peer),
                    label: files::describe(&files::top_level_names(&incoming.files)),
                    total_bytes: incoming.files.iter().map(|file| file.size).sum(),
                }),
            layout_saved: self.layout.is_saved_for(&self.device_monitors()),
            saved_layouts: self.layout.saved_count(),
            rival_programs: self.rivals.clone(),
        }
    }

    fn device_views(&self) -> Vec<DeviceView> {
        let ip_text = |addresses: &[SocketAddr]| -> Vec<String> {
            addresses
                .iter()
                .map(|address| address.ip().to_string())
                .collect()
        };
        let paired = self.paired.values().map(|device| {
            let connected = self
                .sessions
                .get(&device.fingerprint)
                .is_some_and(|session| session.up);
            DeviceView {
                fingerprint: device.fingerprint,
                name: device.name.clone(),
                model: device.model.clone(),
                status: if self.incompatible.contains(&device.fingerprint) {
                    DeviceStatus::Incompatible
                } else if connected && self.peers_off.contains(&device.fingerprint) {
                    DeviceStatus::Off
                } else if connected {
                    DeviceStatus::Connected
                } else {
                    DeviceStatus::Offline
                },
                addresses: ip_text(&self.known_addresses(device)),
                needs_update_for_files: self
                    .sessions
                    .get(&device.fingerprint)
                    .is_some_and(|session| session.up && session.features & FEATURE_FILES == 0),
                can_receive_files: connected
                    && !self.peers_off.contains(&device.fingerprint)
                    && self
                        .sessions
                        .get(&device.fingerprint)
                        .is_some_and(|session| session.features & FEATURE_SEND_FILES != 0),
                files_allowed: device.files_allowed,
                screens_allowed: device.screens_allowed,
                watching_screens: self.watchers.contains_key(&device.fingerprint),
            }
        });
        let available = self
            .discovered
            .values()
            .filter(|found| !self.paired.contains_key(&found.fingerprint))
            .map(|found| DeviceView {
                fingerprint: found.fingerprint,
                name: found.name.clone(),
                model: found.model.clone(),
                status: DeviceStatus::Available,
                addresses: ip_text(&found.addresses),
                needs_update_for_files: false,
                can_receive_files: false,
                files_allowed: false,
                screens_allowed: false,
                watching_screens: false,
            });
        paired.chain(available).collect()
    }
}

/// A picture of an app, as the app's page can show it.
fn icon_url(png: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    format!("data:image/png;base64,{encoded}")
}

/// Drops monitors that make no sense. All the layout geometry divides by their
/// sizes, and they come from another device.
fn sanitize(mut monitors: Vec<Monitor>) -> Vec<Monitor> {
    monitors.retain(Monitor::is_valid);
    monitors.truncate(MAX_MONITORS);
    monitors
}

/// Reads "192.168.1.20" or "192.168.1.20:24850".
fn parse_address(text: &str, default_port: u16) -> Option<SocketAddr> {
    let text = text.trim();
    text.parse().ok().or_else(|| {
        text.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, default_port))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_work_with_or_without_a_port() {
        let with_port: SocketAddr = "192.168.1.20:5000".parse().unwrap();
        let default: SocketAddr = "192.168.1.20:24850".parse().unwrap();
        assert_eq!(parse_address("192.168.1.20:5000", 24850), Some(with_port));
        assert_eq!(parse_address("  192.168.1.20 ", 24850), Some(default));
        assert_eq!(parse_address("not an address", 24850), None);
        assert_eq!(parse_address("", 24850), None);
    }
}
