import { getCurrentWindow } from "@tauri-apps/api/window";
import { create } from "zustand";

export type Theme = "light" | "dark";
export type ThemeChoice = Theme | "system";

const STORAGE_KEY = "hackerman-theme";

const systemDark = window.matchMedia("(prefers-color-scheme: dark)");

/** What the user chose, if they ever did. Until then, the app follows the system. */
function chosenTheme(): Theme | null {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return stored === "light" || stored === "dark" ? stored : null;
  } catch {
    return null;
  }
}

function systemTheme(): Theme {
  return systemDark.matches ? "dark" : "light";
}

export const useTheme = create<{ theme: Theme; choice: ThemeChoice }>(() => ({
  theme: chosenTheme() ?? systemTheme(),
  choice: chosenTheme() ?? "system",
}));

function show(theme: Theme) {
  document.documentElement.classList.toggle("dark", theme === "dark");
  useTheme.setState({ theme });
}

/** Colours the window's title bar. Without a choice, it keeps following the system. */
function showInTitleBar(theme: Theme | null) {
  void getCurrentWindow()
    .setTheme(theme)
    .catch(() => {});
}

/** Call once, before anything is drawn. */
export function startTheme(): void {
  const chosen = chosenTheme();
  show(chosen ?? systemTheme());
  if (chosen) showInTitleBar(chosen);
  systemDark.addEventListener("change", () => {
    if (!chosenTheme()) show(systemTheme());
  });
}

export function chooseTheme(choice: ThemeChoice): void {
  try {
    if (choice === "system") {
      localStorage.removeItem(STORAGE_KEY);
    } else {
      localStorage.setItem(STORAGE_KEY, choice);
    }
  } catch {
    // Still applies for now, and the system's setting returns on the next start.
  }
  useTheme.setState({ choice });
  show(choice === "system" ? systemTheme() : choice);
  showInTitleBar(choice === "system" ? null : choice);
}
