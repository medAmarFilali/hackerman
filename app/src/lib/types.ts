// The agent's messages, matching crates/hk-ipc/src/lib.rs.

export type Fingerprint = string;

export interface Placement {
  device: Fingerprint;
  monitor: string;
  x: number;
  y: number;
  /** A width chosen by the user. Without one, the size comes from the screen. */
  width?: number | null;
}

export type Request =
  | { type: "pair"; device: Fingerprint }
  | { type: "pairAddress"; address: string }
  | { type: "submitPairingCode"; code: string }
  | { type: "dismissPairing" }
  | { type: "forget"; device: Fingerprint }
  | { type: "setPlacements"; placements: Placement[] }
  | { type: "rename"; name: string }
  | { type: "setRules"; rules: Rules }
  | { type: "setLocked"; locked: boolean }
  | { type: "setEnabled"; enabled: boolean }
  | { type: "sendFiles"; device: Fingerprint; paths: string[] }
  | { type: "cancelTransfer"; id: string }
  | { type: "dismissTransfer"; id: string }
  | { type: "answerIncomingFiles"; id: string; accept: boolean; always?: boolean }
  | { type: "setFilesAllowed"; device: Fingerprint; allowed: boolean }
  | { type: "mediaCommand"; device: Fingerprint; app: string; command: MediaCommand }
  | { type: "recordingShortcut"; active: boolean }
  | { type: "appAction"; device: Fingerprint; app: string; action: AppAction }
  | { type: "setScreensAllowed"; device: Fingerprint; allowed: boolean }
  | { type: "shutdown" };

export type MediaCommand = "playPause" | "back" | "forward";

/** Something to do with an app's windows, on the PC it's on. */
export type AppAction =
  | { bringUp: { monitor: string | null } }
  | { moveTo: { from: string; to: string } };

/** Which picture of which screen. */
export interface ScreenAsk {
  device: Fingerprint;
  monitor: string;
  maxWidth: number;
  maxHeight: number;
  /** The fingerprint of the picture already on show, or zero. */
  known: number;
  cursor: boolean;
}

export type PictureAnswer =
  | { status: "picture"; url: string; width: number; height: number; fingerprint: number }
  | { status: "unchanged" }
  | { status: "notAllowed" }
  | { status: "needsUpdate" }
  | { status: "unavailable" }
  | { status: "busy" };

/** A key and the modifiers held with it. With no key, it's turned off. */
export interface Shortcut {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
  /** Which physical key, like "Space" or "KeyQ". */
  key?: string | null;
}

export interface MediaShortcuts {
  enabled: boolean;
  playPause: Shortcut;
  back: Shortcut;
  forward: Shortcut;
}

export type CrossModifier = "none" | "ctrl" | "shift" | "alt";
export type ReceiveFiles = "askFirst" | "anyDevice" | "ask" | "never";
export type NameClash = "keepBoth" | "replace" | "skip";

export interface Rules {
  clipboard: boolean;
  shortcuts: boolean;
  stayInFullscreen: boolean;
  edgeDelayMs: number;
  crossModifier: CrossModifier;
  cornerGuard: number;
  receiveFiles: ReceiveFiles;
  /** Where files from other PCs go. Without one, the Desktop. */
  dropFolder?: string | null;
  nameClash: NameClash;
  notifyFiles: boolean;
  /** Tell other PCs the names of the apps open here. */
  shareApps: boolean;
  /** Let other PCs play, pause and skip what's playing here. */
  mediaControl: boolean;
  /** Let other PCs bring apps here to the front, and move them between the screens here. */
  appControl: boolean;
  mediaShortcuts: MediaShortcuts;
}

/** Whether this PC is set up to work at the lock screen. */
export interface LockScreenStatus {
  available: boolean;
  on: boolean;
  running: boolean;
  upToDate: boolean;
  /** Paired PCs that can't reach the lock screen until they're allowed. */
  notAllowedYet: string[];
}

export type TransferStage =
  | { stage: "waiting" }
  | { stage: "running" }
  | { stage: "done" }
  | { stage: "failed"; reason: string }
  | { stage: "cancelled" };

export interface TransferView {
  id: string;
  /** The other PC. It's this PC for files saved here from here. */
  peer: Fingerprint;
  sending: boolean;
  label: string;
  totalBytes: number;
  doneBytes: number;
  stage: TransferStage;
  folder: string | null;
}

export interface IncomingFilesView {
  id: string;
  peerName: string;
  label: string;
  totalBytes: number;
}

export interface ThisDevice {
  fingerprint: Fingerprint;
  name: string;
  version: string;
  model: string | null;
  addresses: string[];
  port: number;
}

export type DeviceStatus = "connected" | "off" | "offline" | "incompatible" | "available";

export interface DeviceView {
  fingerprint: Fingerprint;
  name: string;
  status: DeviceStatus;
  model: string | null;
  addresses: string[];
  needsUpdateForFiles: boolean;
  canReceiveFiles: boolean;
  /** It can send files here without asking. */
  filesAllowed: boolean;
  /** It can see this PC's screens. */
  screensAllowed: boolean;
  /** It has just been looking at them. */
  watchingScreens: boolean;
}

export interface ScreenView {
  device: Fingerprint;
  monitor: string;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  customWidth: number | null;
  pixelWidth: number;
  pixelHeight: number;
  scale: number;
  primary: boolean;
  online: boolean;
  /** What's open on this screen, frontmost first. */
  apps: AppView[];
}

export interface MediaState {
  /** Whether it's playing right now. Some apps don't say. */
  playing: boolean | null;
  canGoBack: boolean;
  canGoForward: boolean;
}

export interface AppView {
  id: string;
  name: string;
  /** A picture of it, ready to show. Some apps have none to be found. */
  icon: string | null;
  minimized: boolean;
  /** Set if it can be played and paused from here. */
  media: MediaState | null;
  /** It has a window to bring up or move. One playing from the tray doesn't. */
  hasWindow: boolean;
}

export type PairingStage =
  | { stage: "connecting" }
  | { stage: "enterCode" }
  | { stage: "showCode"; code: string }
  | { stage: "verifying" }
  | { stage: "paired" }
  | { stage: "failed"; reason: string };

export interface PairingView {
  peerName: string | null;
  stage: PairingStage;
}

export type ControlView =
  | { state: "home" }
  | { state: "controlling"; device: Fingerprint }
  | { state: "controlledBy"; device: Fingerprint };

export interface UiState {
  me: ThisDevice;
  devices: DeviceView[];
  screens: ScreenView[];
  pairing: PairingView | null;
  control: ControlView;
  rules: Rules;
  enabled: boolean;
  locked: boolean;
  heldByFullscreen: boolean;
  captureError: string | null;
  desktopFolder: string | null;
  transfers: TransferView[];
  incomingFiles: IncomingFilesView | null;
  /** Whether a layout is saved for exactly the screens there are now. */
  layoutSaved: boolean;
  /** How many sets of screens have a layout saved. */
  savedLayouts: number;
  /** Other programs running here that share the keyboard and mouse too. */
  rivalPrograms: string[];
}
