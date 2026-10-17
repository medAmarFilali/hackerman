import { useEffect, useState } from "react";

import type { MediaShortcuts, Shortcut } from "@/lib/types";

/** Keys are stored by where they are, so what's printed on them depends on the keyboard. */
type KeyLabels = ReadonlyMap<string, string>;

const NO_LABELS: KeyLabels = new Map();

const MODIFIERS = new Set([
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "ShiftLeft",
  "ShiftRight",
  "MetaLeft",
  "MetaRight",
]);

const KEY_NAMES: Record<string, string> = {
  ArrowLeft: "Left",
  ArrowRight: "Right",
  ArrowUp: "Up",
  ArrowDown: "Down",
  PageUp: "Page Up",
  PageDown: "Page Down",
  MediaPlayPause: "Play/Pause key",
  MediaTrackNext: "Next track key",
  MediaTrackPrevious: "Previous track key",
  MediaStop: "Stop key",
  AudioVolumeUp: "Volume up key",
  AudioVolumeDown: "Volume down key",
  AudioVolumeMute: "Mute key",
};

/** The Ctrl+Alt ones Hackerman has for itself, whatever else is chosen. */
const ALREADY_TAKEN = new Set([
  "KeyL",
  "Home",
  "Digit1",
  "Digit2",
  "Digit3",
  "Digit4",
  "Digit5",
  "Digit6",
  "Digit7",
  "Digit8",
  "Digit9",
]);

export const USUAL_MEDIA_SHORTCUTS: Omit<MediaShortcuts, "enabled"> = {
  playPause: { ctrl: true, alt: true, shift: false, meta: false, key: "Space" },
  back: { ctrl: true, alt: true, shift: false, meta: false, key: "PageUp" },
  forward: { ctrl: true, alt: true, shift: false, meta: false, key: "PageDown" },
};

/** What a shortcut set to nothing looks like. */
export const NO_SHORTCUT: Shortcut = { ctrl: false, alt: false, shift: false, meta: false };

export function isModifierKey(code: string): boolean {
  return MODIFIERS.has(code);
}

/** The shortcut being pressed, for a key that isn't itself a modifier. */
export function shortcutFromEvent(
  event: Pick<KeyboardEvent, "code" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
): Shortcut {
  return {
    ctrl: event.ctrlKey,
    alt: event.altKey,
    shift: event.shiftKey,
    meta: event.metaKey,
    key: event.code,
  };
}

function keyName(code: string, labels: KeyLabels): string {
  const printed = labels.get(code);
  if (printed && printed.trim()) return printed.toUpperCase();
  if (code in KEY_NAMES) return KEY_NAMES[code];
  if (code.startsWith("Key")) return code.slice(3);
  if (code.startsWith("Digit")) return code.slice(5);
  return code;
}

export function describeShortcut(shortcut: Shortcut, labels: KeyLabels = NO_LABELS): string {
  if (!shortcut.key) return "Off";
  return [
    shortcut.ctrl && "Ctrl",
    shortcut.alt && "Alt",
    shortcut.shift && "Shift",
    shortcut.meta && "Win",
    keyName(shortcut.key, labels),
  ]
    .filter(Boolean)
    .join("+");
}

function sameShortcut(a: Shortcut, b: Shortcut): boolean {
  return (
    a.key === b.key && a.ctrl === b.ctrl && a.alt === b.alt && a.shift === b.shift && a.meta === b.meta
  );
}

/** Keys with no typing to get in the way of. */
function standsAlone(code: string): boolean {
  return /^F\d+$/.test(code) || code.startsWith("Media") || code.startsWith("AudioVolume");
}

/** Why this can't be a shortcut, given the others already chosen. */
export function shortcutProblem(shortcut: Shortcut, others: Shortcut[]): string | null {
  const { key } = shortcut;
  if (!key) return null;
  if (shortcut.ctrl && shortcut.alt && !shortcut.shift && !shortcut.meta && ALREADY_TAKEN.has(key)) {
    return "Hackerman already uses that for something else.";
  }
  if (!shortcut.ctrl && !shortcut.alt && !shortcut.meta && !standsAlone(key)) {
    return "Hold Ctrl, Alt or the Windows key too, or that key couldn't be typed any more.";
  }
  if (others.some((other) => sameShortcut(other, shortcut))) {
    return "That's already used for one of the others.";
  }
  return null;
}

let labelsLoaded: Promise<KeyLabels> | null = null;

function loadLabels(): Promise<KeyLabels> {
  const keyboard = (
    navigator as Navigator & { keyboard?: { getLayoutMap(): Promise<KeyLabels> } }
  ).keyboard;
  labelsLoaded ??= keyboard ? keyboard.getLayoutMap().catch(() => NO_LABELS) : Promise.resolve(NO_LABELS);
  return labelsLoaded;
}

/** What's printed on this computer's keys, once it's known. On a French keyboard, the key the
 * rest of the world calls Q says A. */
export function useKeyLabels(): KeyLabels {
  const [labels, setLabels] = useState(NO_LABELS);
  useEffect(() => {
    void loadLabels().then(setLabels);
  }, []);
  return labels;
}
