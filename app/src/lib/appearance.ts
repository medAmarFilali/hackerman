import { create } from "zustand";

/** The parts of the drawings on the canvas that can be left out. */
export interface Appearance {
  /** What's drawn under a laptop's screen. */
  laptopKeyboards: boolean;
  /** What's drawn under a monitor. */
  monitorStands: boolean;
}

const STORAGE_KEY = "hackerman-appearance";

function saved(): Appearance {
  let stored: Partial<Appearance> | null = null;
  try {
    stored = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
  } catch {
    // Everything's drawn, as it is for someone who has never changed these.
  }
  return {
    laptopKeyboards: stored?.laptopKeyboards !== false,
    monitorStands: stored?.monitorStands !== false,
  };
}

export const useAppearance = create<Appearance>(() => saved());

export function changeAppearance(change: Partial<Appearance>): void {
  useAppearance.setState(change);
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(useAppearance.getState()));
  } catch {
    // Still applies for now, and goes back to the saved choice on the next start.
  }
}
