import { describe, expect, it } from "vitest";

import {
  NO_SHORTCUT,
  USUAL_MEDIA_SHORTCUTS,
  describeShortcut,
  isModifierKey,
  shortcutFromEvent,
  shortcutProblem,
} from "@/lib/shortcuts";
import type { Shortcut } from "@/lib/types";

function shortcut(key: string, change: Partial<Shortcut> = {}): Shortcut {
  return { ...NO_SHORTCUT, key, ...change };
}

const CTRL_ALT = { ctrl: true, alt: true };

describe("describeShortcut", () => {
  it("says what to press", () => {
    expect(describeShortcut(USUAL_MEDIA_SHORTCUTS.playPause)).toBe("Ctrl+Alt+Space");
    expect(describeShortcut(USUAL_MEDIA_SHORTCUTS.forward)).toBe("Ctrl+Alt+Page Down");
    expect(describeShortcut(shortcut("KeyP", { meta: true, shift: true }))).toBe("Shift+Win+P");
    expect(describeShortcut(shortcut("Digit5", { alt: true }))).toBe("Alt+5");
    expect(describeShortcut(shortcut("F13"))).toBe("F13");
    expect(describeShortcut(shortcut("MediaPlayPause"))).toBe("Play/Pause key");
  });

  it("says when it's off", () => {
    expect(describeShortcut(NO_SHORTCUT)).toBe("Off");
    expect(describeShortcut({ ...NO_SHORTCUT, key: null })).toBe("Off");
  });

  it("goes by what's printed on the keys of this keyboard", () => {
    const french = new Map([
      ["KeyQ", "a"],
      ["Digit1", "&"],
    ]);
    expect(describeShortcut(shortcut("KeyQ", CTRL_ALT), french)).toBe("Ctrl+Alt+A");
    expect(describeShortcut(shortcut("Digit1", CTRL_ALT), french)).toBe("Ctrl+Alt+&");
    expect(describeShortcut(shortcut("KeyP", CTRL_ALT), french)).toBe("Ctrl+Alt+P");
    // A space on the map isn't anything to show for the name of a key.
    expect(describeShortcut(shortcut("Space", CTRL_ALT), new Map([["Space", " "]]))).toBe(
      "Ctrl+Alt+Space",
    );
  });
});

describe("shortcutProblem", () => {
  it("accepts the usual ones", () => {
    for (const usual of Object.values(USUAL_MEDIA_SHORTCUTS)) {
      expect(shortcutProblem(usual, [])).toBeNull();
    }
  });

  it("refuses what Hackerman already uses", () => {
    for (const key of ["KeyL", "Digit1", "Digit9", "Home"]) {
      expect(shortcutProblem(shortcut(key, CTRL_ALT), [])).toMatch(/already uses/);
    }
    // With something more held, those are free.
    expect(shortcutProblem(shortcut("KeyL", { ...CTRL_ALT, shift: true }), [])).toBeNull();
    expect(shortcutProblem(shortcut("KeyL", { ctrl: true }), [])).toBeNull();
  });

  it("refuses a key that's needed for typing", () => {
    expect(shortcutProblem(shortcut("KeyA"), [])).toMatch(/typed/);
    expect(shortcutProblem(shortcut("Space"), [])).toMatch(/typed/);
    expect(shortcutProblem(shortcut("KeyA", { shift: true }), [])).toMatch(/typed/);
  });

  it("accepts a key with no typing to get in the way of", () => {
    for (const key of ["F13", "F5", "MediaPlayPause", "MediaTrackNext", "AudioVolumeUp"]) {
      expect(shortcutProblem(shortcut(key), [])).toBeNull();
    }
  });

  it("refuses one that's taken by another", () => {
    const taken = shortcut("KeyP", CTRL_ALT);
    expect(shortcutProblem(shortcut("KeyP", CTRL_ALT), [taken])).toMatch(/already used/);
    expect(shortcutProblem(shortcut("KeyP", { ...CTRL_ALT, shift: true }), [taken])).toBeNull();
    expect(shortcutProblem(shortcut("KeyO", CTRL_ALT), [taken])).toBeNull();
  });

  it("has nothing to say about one that's off", () => {
    expect(shortcutProblem(NO_SHORTCUT, [NO_SHORTCUT])).toBeNull();
  });
});

describe("recording", () => {
  it("reads what's held from the key press", () => {
    expect(
      shortcutFromEvent({
        code: "KeyN",
        ctrlKey: true,
        altKey: false,
        shiftKey: true,
        metaKey: false,
      }),
    ).toEqual({ ctrl: true, alt: false, shift: true, meta: false, key: "KeyN" });
  });

  it("waits for a real key after the modifiers", () => {
    for (const code of ["ControlLeft", "AltRight", "ShiftLeft", "MetaRight"]) {
      expect(isModifierKey(code)).toBe(true);
    }
    expect(isModifierKey("KeyA")).toBe(false);
    expect(isModifierKey("Space")).toBe(false);
  });
});
