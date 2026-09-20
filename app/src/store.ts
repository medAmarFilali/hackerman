import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { create } from "zustand";

import type { LockScreenStatus, Request, UiState } from "@/lib/types";

interface AppStore {
  /** The latest state from the background agent, once it has sent one. */
  state: UiState | null;
  /** Why the app can't reach the agent, if that's the case. */
  agentProblem: string | null;
}

export const useApp = create<AppStore>(() => ({
  state: null,
  agentProblem: null,
}));

export function send(request: Request): void {
  void invoke("send_request", { request });
}

/** Shows a folder in File Explorer. */
export function openFolder(path: string): void {
  void invoke("open_folder", { path });
}

export function openLogFolder(): void {
  void invoke("open_log_folder");
}

export function lockScreenStatus(): Promise<LockScreenStatus> {
  return invoke("lock_screen_status");
}

/** Windows asks the user first. Says whether it happened, and fails if it went wrong. */
export function setLockScreen(on: boolean): Promise<boolean> {
  return invoke("set_lock_screen", { on });
}

/** Starts following the agent. Call once, when the app starts. */
export async function connectToAgent(): Promise<void> {
  await listen<UiState>("agent-state", (event) => {
    useApp.setState({ state: event.payload, agentProblem: null });
  });
  await listen<string>("agent-problem", (event) => {
    useApp.setState({ agentProblem: event.payload });
  });
  // The agent may have reported before these listeners existed.
  const latest = await invoke<UiState | null>("current_state");
  if (latest) {
    useApp.setState({ state: latest });
  }
}
