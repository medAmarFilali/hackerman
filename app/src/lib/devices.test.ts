import { describe, expect, it } from "vitest";

import {
  activeDevice,
  cantReceiveFilesBecause,
  deviceNumbers,
  headline,
  isLaptop,
} from "./devices";
import type { DeviceStatus, UiState } from "./types";

const ME = "me";

function device(fingerprint: string, status: DeviceStatus = "connected") {
  return {
    fingerprint,
    name: fingerprint.toUpperCase(),
    status,
    model: null,
    addresses: [],
    needsUpdateForFiles: false,
    canReceiveFiles: true,
    filesAllowed: false,
  screensAllowed: false,
  watchingScreens: false,
  };
}

function stateWith(change: Partial<UiState> = {}): UiState {
  return {
    me: { fingerprint: ME, name: "Me", version: "1", model: null, addresses: [], port: 1 },
    devices: [],
    screens: [],
    pairing: null,
    control: { state: "home" },
    rules: {
      clipboard: true,
      shortcuts: true,
      stayInFullscreen: false,
      edgeDelayMs: 0,
      crossModifier: "none",
      cornerGuard: 0,
      receiveFiles: "askFirst",
      nameClash: "keepBoth",
      notifyFiles: true,
      shareApps: true,
      mediaControl: true,
      appControl: true,
      mediaShortcuts: {
        enabled: true,
        playPause: { ctrl: true, alt: true, shift: false, meta: false, key: "Space" },
        back: { ctrl: true, alt: true, shift: false, meta: false, key: "PageUp" },
        forward: { ctrl: true, alt: true, shift: false, meta: false, key: "PageDown" },
      },
    },
    enabled: true,
    locked: false,
    heldByFullscreen: false,
    captureError: null,
    desktopFolder: null,
    transfers: [],
    incomingFiles: null,
    layoutSaved: true,
    savedLayouts: 1,
    rivalPrograms: [],
    ...change,
  };
}

describe("deviceNumbers", () => {
  it("numbers this PC first, then paired PCs in order, as the shortcuts do", () => {
    const numbers = deviceNumbers(
      stateWith({ devices: [device("b"), device("c", "offline"), device("d", "off")] }),
    );
    expect([...numbers]).toEqual([
      [ME, 1],
      ["b", 2],
      ["c", 3],
      ["d", 4],
    ]);
  });

  it("leaves out PCs that aren't paired", () => {
    const numbers = deviceNumbers(stateWith({ devices: [device("x", "available"), device("b")] }));
    expect(numbers.get("x")).toBeUndefined();
    expect(numbers.get("b")).toBe(2);
  });
});

describe("cantReceiveFilesBecause", () => {
  it("lets files go to this PC and to connected PCs that can take them", () => {
    const state = stateWith({ devices: [device("b")] });
    expect(cantReceiveFilesBecause(state, ME)).toBeNull();
    expect(cantReceiveFilesBecause(state, "b")).toBeNull();
  });

  it("says why not otherwise", () => {
    const older = { ...device("old"), canReceiveFiles: false };
    const state = stateWith({
      devices: [older, device("away", "offline"), device("off", "off"), device("v", "incompatible")],
    });
    expect(cantReceiveFilesBecause(state, "old")).toContain("updating");
    expect(cantReceiveFilesBecause(state, "away")).toContain("connected");
    expect(cantReceiveFilesBecause(state, "off")).toContain("disabled");
    expect(cantReceiveFilesBecause(state, "v")).toContain("version");
    expect(cantReceiveFilesBecause(state, "unknown")).toContain("connected");
  });

  it("stops everything while this PC is disabled", () => {
    const state = stateWith({ enabled: false, devices: [device("b")] });
    expect(cantReceiveFilesBecause(state, "b")).toContain("disabled here");
    expect(cantReceiveFilesBecause(state, ME)).toContain("disabled here");
  });
});

describe("activeDevice", () => {
  it("is this PC unless it's controlling another", () => {
    expect(activeDevice(stateWith())).toBe(ME);
    expect(activeDevice(stateWith({ control: { state: "controlling", device: "b" } }))).toBe("b");
    expect(activeDevice(stateWith({ control: { state: "controlledBy", device: "b" } }))).toBe(ME);
  });

  it("is nothing while sharing is off", () => {
    expect(activeDevice(stateWith({ enabled: false }))).toBeNull();
  });
});

describe("isLaptop", () => {
  const screen = (device: string, name: string) => ({
    device,
    monitor: name,
    name,
    x: 0,
    y: 0,
    width: 1,
    height: 1,
    customWidth: null,
    pixelWidth: 1,
    pixelHeight: 1,
    scale: 1,
    primary: true,
    online: true,
    apps: [],
  });

  it("goes by whether a PC has a built-in display", () => {
    const state = stateWith({
      screens: [screen("a", "Built-in display"), screen("a", "DELL U2720Q"), screen("b", "DELL U2720Q")],
    });
    expect(isLaptop(state, "a")).toBe(true);
    expect(isLaptop(state, "b")).toBe(false);
  });
});

describe("headline", () => {
  it("puts being disabled above everything else", () => {
    const { title } = headline(
      stateWith({ enabled: false, locked: true, control: { state: "controlling", device: "b" } }),
    );
    expect(title).toBe("Hackerman is disabled");
  });

  it("names who's in control", () => {
    const devices = [device("b")];
    expect(headline(stateWith({ devices, control: { state: "controlling", device: "b" } })).title).toBe(
      "Controlling B",
    );
    expect(headline(stateWith({ devices, control: { state: "controlledBy", device: "b" } })).title).toBe(
      "B is controlling this PC",
    );
  });

  it("copes with a PC it no longer knows", () => {
    expect(headline(stateWith({ control: { state: "controlledBy", device: "gone" } })).title).toBe(
      "Another PC is controlling this PC",
    );
  });

  it("says why the cursor is staying", () => {
    expect(headline(stateWith({ heldByFullscreen: true })).detail).toContain("full-screen");
    expect(headline(stateWith({ locked: true })).detail).toContain("locked");
  });

  it("points to pairing when there's nothing to cross to", () => {
    expect(headline(stateWith({ devices: [device("x", "available")] })).detail).toContain("Pair");
    expect(headline(stateWith({ devices: [device("b")] })).detail).toContain("violet band");
  });
});
