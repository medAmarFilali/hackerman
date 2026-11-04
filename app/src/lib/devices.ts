import type { Fingerprint, ScreenView, UiState } from "./types";

/** The name the agent gives a laptop's own screen. */
const BUILT_IN_DISPLAY = "Built-in display";

export function isBuiltIn(screen: Pick<ScreenView, "name">): boolean {
  return screen.name === BUILT_IN_DISPLAY;
}

export function isLaptop(state: UiState, device: Fingerprint): boolean {
  return state.screens.some((screen) => screen.device === device && isBuiltIn(screen));
}

/**
 * The number each PC goes by: this one is 1 and paired ones follow in order, which
 * is also the number that jumps to it with Ctrl+Alt.
 */
export function deviceNumbers(state: UiState): Map<Fingerprint, number> {
  const numbers = new Map<Fingerprint, number>();
  numbers.set(state.me.fingerprint, 1);
  for (const device of state.devices) {
    if (device.status !== "available") {
      numbers.set(device.fingerprint, numbers.size + 1);
    }
  }
  return numbers;
}

/** The name a PC goes by on the canvas. */
export function nameOf(state: UiState, device: Fingerprint): string {
  return device === state.me.fingerprint
    ? "This PC"
    : (state.devices.find((d) => d.fingerprint === device)?.name ?? "Unknown");
}

/**
 * The PCs a monitor plugged into several of them can be showing, this one first, then in the
 * order they're numbered. Empty for any other monitor.
 */
export function sharedChoices(
  state: UiState,
  screen: Pick<ScreenView, "shared">,
): { device: Fingerprint; name: string }[] {
  const numbers = deviceNumbers(state);
  const order = (device: Fingerprint) => numbers.get(device) ?? Number.MAX_SAFE_INTEGER;
  return [...(screen.shared?.devices ?? [])]
    .sort((a, b) => order(a) - order(b))
    .map((device) => ({ device, name: nameOf(state, device) }));
}

/** The PC the keyboard and mouse are working on, unless sharing is off. */
export function activeDevice(state: UiState): Fingerprint | null {
  if (!state.enabled) return null;
  return state.control.state === "controlling" ? state.control.device : state.me.fingerprint;
}

/** Why this PC can't be sent files right now, if it can't. */
export function cantReceiveFilesBecause(state: UiState, device: Fingerprint): string | null {
  if (!state.enabled) return "Hackerman is disabled here";
  if (device === state.me.fingerprint) return null;
  const found = state.devices.find((candidate) => candidate.fingerprint === device);
  switch (found?.status) {
    case "connected":
      return found.canReceiveFiles ? null : "It needs updating first";
    case "off":
      return "Hackerman is disabled there";
    case "incompatible":
      return "It's on a different version";
    default:
      return "It isn't connected";
  }
}

export interface Headline {
  title: string;
  detail: string;
}

/** What's happening now, for the top of the window. */
export function headline(state: UiState): Headline {
  const nameOf = (device: Fingerprint) =>
    state.devices.find((d) => d.fingerprint === device)?.name ?? "Another PC";

  if (!state.enabled) {
    return {
      title: "Hackerman is disabled",
      detail: "This PC keeps its own keyboard and mouse, and nothing is shared with other PCs.",
    };
  }
  if (state.control.state === "controlling") {
    return {
      title: `Controlling ${nameOf(state.control.device)}`,
      detail: "Ctrl+Alt+Home brings your keyboard and mouse back at any time.",
    };
  }
  if (state.control.state === "controlledBy") {
    return {
      title: `${nameOf(state.control.device)} is controlling this PC`,
      detail: "Its keyboard and mouse are working here. Moving the cursor back returns them.",
    };
  }
  if (state.heldByFullscreen) {
    return {
      title: "Staying on this PC",
      detail: "A full-screen app is in front, so the cursor won't cross until it's gone.",
    };
  }
  if (state.locked) {
    return {
      title: "Staying on this PC",
      detail: "The cursor is locked here until you unlock it.",
    };
  }
  const paired = state.devices.some((device) => device.status !== "available");
  return {
    title: "Your keyboard and mouse are on this PC",
    detail: paired
      ? "Push the cursor through a violet band to cross to another PC."
      : "Pair another PC from the list on the right to start sharing them.",
  };
}
